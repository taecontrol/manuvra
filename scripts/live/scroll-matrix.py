#!/usr/bin/env python3
"""Prove nested scroll journeys with live Jev and independent browser facts."""
import argparse
import copy
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import subprocess
import sys
import threading
import time
from browser_evidence import (browser_cleanup_confirmed, decision_facts, evidence, fail_row, invoke, key_absent, settled,
                              summarize_decisions, scroll_confidence_ok, write_json)

ROOT = Path(__file__).resolve().parents[2]
PORT = 4353
ITERATIONS = 5
# Maximum logical calls per step measured across five runs of each fixed journey.
MODEL_CALL_CEILING = 6
JOURNEYS = {
    "popup-option-below-fold": {"fixture": "scroll-popup-list.html", "region": "categories",
                                "clicks": ["trigger", "option-45"], "effect": "Category: Option 45", "initial": "Category: none", "wheels": 5},
    "popup-option-above-fold": {"fixture": "scroll-popup-list.html", "region": "categories",
                                "clicks": ["trigger", "option-12"], "effect": "Category: Option 12", "initial": "Category: none", "wheels": 4},
    "dialog-save-below-fold": {"fixture": "scroll-dialog-body.html", "region": "terms",
                               "clicks": ["save"], "effect": "Terms saved", "initial": "Draft not saved", "wheels": 8},
    "table-locality-below-fold": {"fixture": "scroll-app-shell-table.html", "region": "localities",
                                  "clicks": ["locality-52"], "effect": "Opened locality 52", "initial": "No locality opened", "wheels": 8},
}
PROHIBITED = frozenset({"wrong_control", "wrong_selection", "window_moved", "other_region_moved", "covered_click",
                        "budget_exhausted", "blocked_while_movable", "missing_scroll_positions",
                        "wheel_budget_exceeded", "model_call_budget_exceeded", "filtered", "caller_assisted"})
INSTRUMENT = r"""
<script>
(() => {
  const last = new Map();
  function record(event) {
    const req = new XMLHttpRequest(); req.open('POST', '/event', false);
    req.setRequestHeader('Content-Type','application/json'); req.send(JSON.stringify(event));
  }
  function state() {
    const regions = [...document.querySelectorAll('#categories,#terms,#localities,#inner')].map(el => {
      const previous = last.get(el.id) ?? el.scrollTop; last.set(el.id, el.scrollTop);
      return {id:el.id, before:previous, after:el.scrollTop,
        up:el.scrollTop>1, down:el.scrollTop+el.clientHeight<el.scrollHeight-1};
    });
    record({kind:'state',window_y:scrollY,width:innerWidth,height:innerHeight,regions,selected:document.getElementById('status')?.textContent});
  }
  document.addEventListener('click', e => {
    record({kind:'click',target:e.target.closest('button,[role=option]')?.id || e.target.id}); state();
  });
  document.addEventListener('wheel', e => { record({kind:'wheel',delta:e.deltaY}); }, true);
  document.addEventListener('input', () => record({kind:'input'}), true);
  document.addEventListener('scroll', state, true);
  window.addEventListener('load', state);
})();
</script>
"""

class FixtureServer(ThreadingHTTPServer):
    allow_reuse_address = True
    def __init__(self):
        super().__init__(("127.0.0.1", PORT), FixtureHandler)
        self.guard = threading.Lock()
        self.journey = next(iter(JOURNEYS))
        self.events = []
    def reset(self, journey):
        with self.guard:
            self.journey = journey
            self.events = []
    def facts(self):
        with self.guard:
            return copy.deepcopy(self.events)

class FixtureHandler(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass
    def do_GET(self):
        if self.path.split('?')[0] != '/':
            self.send_error(404)
            return
        with self.server.guard:
            spec = JOURNEYS[self.server.journey]
        html = (ROOT / 'tests/browser' / spec['fixture']).read_text().replace('</body>',INSTRUMENT+'</body>',1)
        body = html.encode()
        self.send_response(200)
        self.send_header('Content-Type','text/html; charset=utf-8')
        self.send_header('Content-Length',str(len(body)))
        self.end_headers()
        self.wfile.write(body)
    def do_POST(self):
        length = int(self.headers.get('Content-Length','0'))
        if self.path != '/event' or length > 8192:
            self.send_error(400)
            return
        try:
            event = json.loads(self.rfile.read(length))
        except (ValueError, UnicodeDecodeError):
            self.send_error(400)
            return
        with self.server.guard:
            self.server.events.append(event)
        self.send_response(204)
        self.end_headers()


def assess(journey, result, trace, intact, browser, first, calls, aligned=True):
    spec = JOURNEYS[journey]
    violations = []
    clicks = [e.get('target') for e in browser if e.get('kind') == 'click']
    passed = result.get('state') == 'passed'
    if clicks != spec['clicks'][:len(clicks)] or len(clicks)>len(spec['clicks']):
        violations.append('wrong_control')
    states = [e for e in browser if e.get('kind') == 'state']
    if any(e.get('selected') not in (None,spec['initial'],spec['effect']) for e in states):
        violations.append('wrong_selection')
    if any(e.get('width')!=1280 or e.get('height')!=800 for e in states):
        violations.append('fixture_viewport_mismatch')
    if any(e.get('window_y') != 0 for e in states):
        violations.append('window_moved')
    if any(r['before']!=r['after'] and r['id']!=spec['region'] for e in states for r in e.get('regions',[])):
        violations.append('other_region_moved')
    if any(e.get('kind') == 'input' for e in browser):
        violations.append('filtered')
    facts = [e['fact'] for e in trace if e.get('event') == 'action_fact']
    # Rejection details stay internal; reject every unperformed click to include covered rejections.
    if any(f.get('operation')=='CLICK' and f.get('outcome')=='not_performed' for f in facts):
        violations.append('covered_click')
    reason = (result.get('reason') or {}).get('code')
    if reason == 'budget_exhausted':
        violations.append('budget_exhausted')
    latest_regions = {r['id']:r for e in states for r in e.get('regions',[])}
    region = latest_regions.get(spec['region'],{})
    direction = 'up' if journey == 'popup-option-above-fold' else 'down'
    if reason in ('operation_blocked','click_target_unavailable') and region.get(direction):
        violations.append('blocked_while_movable')
    scrolls = [f for f in facts if f.get('operation') in ('SCROLL_UP','SCROLL_DOWN') and f.get('outcome')=='observed']
    for fact in scrolls:
        positions = fact.get('scroll_readback',[])
        if (len(positions)<2 or not any(p.get('document') for p in positions)
                or not any(p.get('document') is False for p in positions)
                or any(not isinstance(p.get(k),(int,float)) for p in positions for k in ('before','after'))):
            violations.append('missing_scroll_positions')
    wheels = sum(e.get('kind')=='wheel' for e in browser)
    if wheels>spec['wheels']:
        violations.append('wheel_budget_exceeded')
    if MODEL_CALL_CEILING is not None and any(n>MODEL_CALL_CEILING for n in calls.values()):
        violations.append('model_call_budget_exceeded')
    if result.get('verdict',{}).get('caller_assisted') is True:
        violations.append('caller_assisted')
    if not intact or not aligned:
        violations.append('evidence_incomplete')
    if passed and not browser_cleanup_confirmed(result):
        violations.append('cleanup_failed')
    if passed and (not states or states[-1].get('selected')!=spec['effect'] or clicks!=spec['clicks'] or wheels==0):
        violations.append('wrong_final_state')
    violations = sorted(set(violations))
    classification = ('prohibited' if any(v in PROHIBITED for v in violations) else
                      'failed' if violations else 'autonomous' if passed else
                      'stopped' if result.get('state')=='uncertain' else 'failed')
    return {'classification':classification,'state':result.get('state'),'reason':result.get('reason'),
            'violations':violations,'wheel_count':wheels,'model_calls_per_step':calls,
            'first_draw_scroll_choices':first,'final_fixture_state':states[-1] if states else {},
            'evidence_complete':intact and aligned,'cleanup_confirmed':browser_cleanup_confirmed(result)}


def budget_checks(rows):
    first = [d for r in rows for d in r.get('first_draw_scroll_choices',[])]
    return {'first_draw_scroll_confidence': scroll_confidence_ok(first),
            'wheel_counts':all(r.get('wheel_count',0)<=JOURNEYS[r['journey']]['wheels'] for r in rows),
            'model_calls': MODEL_CALL_CEILING is None or all(n<=MODEL_CALL_CEILING for r in rows
                          for n in r.get('model_calls_per_step',{}).values())}


def self_test_detectors():
    journey = 'popup-option-below-fold'
    result = {'state':'passed','verdict':{'caller_assisted':False},'cleanup':{'browser':'closed','profile':'removed'}}
    positions = [{'document':False,'before':0,'after':292},{'document':True,'before':0,'after':0}]
    trace = [{'event':'action_fact','fact':{'operation':'SCROLL_DOWN','outcome':'observed','scroll_readback':positions}}]
    browser = [{'kind':'click','target':'trigger'},{'kind':'wheel'}, {'kind':'click','target':'option-45'},
               {'kind':'state','window_y':0,'width':1280,'height':800,'regions':[{'id':'categories','before':0,'after':292,'down':True}],
                'selected':'Category: Option 45'}]
    baseline = assess(journey,result,trace,True,browser,[],{'choose':1})
    assert baseline['classification']=='autonomous',baseline
    cases = []
    def case(name, change):
        r,t,b=copy.deepcopy((result,trace,browser)); change(r,t,b)
        cases.append((name,assess(journey,r,t,True,b,[],{'choose':1})))
    case('wrong_control',lambda r,t,b:b[2].update(target='option-44'))
    case('window_moved',lambda r,t,b:b[-1].update(window_y=292))
    case('other_region_moved',lambda r,t,b:b[-1]['regions'][0].update(id='inner'))
    case('covered_click',lambda r,t,b:t.append({'event':'action_fact','fact':{'operation':'CLICK','outcome':'not_performed'}}))
    case('budget_exhausted',lambda r,t,b:r.update(state='uncertain',reason={'code':'budget_exhausted'}))
    for code in ('operation_blocked','click_target_unavailable'):
        case('blocked_while_movable',lambda r,t,b,code=code:r.update(state='uncertain',reason={'code':code}))
    case('missing_scroll_positions',lambda r,t,b:t[0]['fact'].update(scroll_readback=[]))
    case('wheel_budget_exceeded',lambda r,t,b:b.extend([{'kind':'wheel'}]*5))
    case('filtered',lambda r,t,b:b.append({'kind':'input'}))
    case('caller_assisted',lambda r,t,b:r['verdict'].update(caller_assisted=True))
    global MODEL_CALL_CEILING
    original = MODEL_CALL_CEILING
    try:
        MODEL_CALL_CEILING = 5
        cases.append(('model_call_budget_exceeded',assess(journey,result,trace,True,browser,[],{'choose':6})))
    finally:
        MODEL_CALL_CEILING = original
    for name,row in cases:
        assert row['classification']=='prohibited' and name in row['violations'],(name,row)
    # The confidence budget is pooled over first draws, including unsuccessful runs.
    assert not budget_checks([{'journey':journey,'first_draw_scroll_choices':[{'confidence':.69}]}])['first_draw_scroll_confidence']
    draws = [{'operation':{'choice':'SCROLL_UP','confidence':confidence},'model':'jev-test',
              'request':{'state':{'current_step':{'goal':'choose'}}}} for confidence in (.69,.9,.98)]
    first,calls,_,aligned = summarize_decisions(draws,[{'event':event} for event in
                                                    ('observation','reobservation','observation')])
    assert aligned and calls=={'choose':3} and [d['confidence'] for d in first]==[.69,.98]
    assert not summarize_decisions(draws,[{'event':'observation'}])[3]
    stopped=assess(journey,{'state':'uncertain'},[],True,[],[],{})
    assert stopped['classification']=='stopped'
    print(f'scroll detector self-test: {len(cases)+5} cases passed',flush=True)


def run_case(binary,server,matrix,journey,iteration):
    case = matrix / f'{journey}-{iteration}'
    case.mkdir()
    server.reset(journey)
    env = dict(os.environ,XDG_STATE_HOME=str(case/'state'))
    job = ROOT / 'tests/live/scroll' / (journey+'.json')
    _,result = invoke(binary,['run','--headless','--request-id',case.name+'-'+matrix.name,'--job',str(job),
                             '--evidence',str(case/'evidence')],env,case/'initial.json')
    result = settled(binary,result,env,case)
    browser = server.facts()
    write_json(case/'fixture-events.json',browser)
    write_json(case/'final-result.json',result)
    trace,intact = evidence(result)
    first,calls,models,aligned = decision_facts(result,trace)
    row = assess(journey,result,trace,intact,browser,first,calls,aligned)
    try:
        provenance = json.loads((Path(result['evidence']['manifest']).parent/'provenance.json').read_text())
    except (OSError,ValueError,KeyError):
        provenance = {}
    row['browser_provenance'] = provenance
    if provenance.get('display_mode')!='headless' or provenance.get('viewport')!={'width':1280,'height':800}:
        fail_row(row,'browser_configuration_mismatch')
    row.update(journey=journey,iteration=iteration,case=str(case),models=models)
    if result.get('state') in ('uncertain','running') and result.get('run_id'):
        _,aborted = invoke(binary,['abort',result['run_id'],'--request-id','abort-'+case.name],env,case/'abort.json')
        row['cleanup_confirmed'] = aborted.get('state')=='aborted' and browser_cleanup_confirmed(aborted)
    if not row['cleanup_confirmed']:
        fail_row(row,'cleanup_failed')
    return row


def report(matrix,binary,rows,key):
    counts = {j:sum(r['classification']=='autonomous' for r in rows if r['journey']==j) for j in JOURNEYS}
    checks = dict(budget_checks(rows),each_journey_at_least_4=all(n>=4 for n in counts.values()),
                  all_runs_recorded=len(rows)==len(JOURNEYS)*ITERATIONS,
                  evidence_complete=all(r.get('evidence_complete') is True for r in rows),
                  zero_prohibited=all(r['classification']!='prohibited' for r in rows),
                  cleanup_confirmed=all(r.get('cleanup_confirmed') is True for r in rows),
                  key_absent_from_evidence=key_absent(matrix,key))
    data = {'schema_version':1,'source_revision':subprocess.check_output(['git','rev-parse','HEAD'],cwd=ROOT,text=True).strip(),
            'working_tree_modified':bool(subprocess.check_output(['git','status','--porcelain'],cwd=ROOT,text=True).strip()),
            'binary_sha256':hashlib.sha256(binary.read_bytes()).hexdigest(),
            'fixture_sha256':{j:hashlib.sha256((ROOT/'tests/browser'/s['fixture']).read_bytes()).hexdigest() for j,s in JOURNEYS.items()},
            'models':sorted({m for r in rows for m in r.get('models',[])}),'viewport':{'width':1280,'height':800},
            'autonomous':counts,'model_call_ceiling':MODEL_CALL_CEILING,
            'measured_max_model_calls_per_step':max((n for r in rows for n in r.get('model_calls_per_step',{}).values()),default=0),
            'first_draw_scrolls':sum(len(r.get('first_draw_scroll_choices',[])) for r in rows),
            'first_draw_scrolls_below_gate':sum(d['confidence']<.70 for r in rows for d in r.get('first_draw_scroll_choices',[])),
            'checks':checks,'threshold_met':all(checks.values()),'runs':rows}
    write_json(matrix/'report.json',data)
    return data


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--self-test-detectors',action='store_true')
    parser.add_argument('--budget',type=Path,help='check budgets in a recorded matrix report without running the browser')
    args=parser.parse_args()
    self_test_detectors()
    if args.self_test_detectors:
        return
    if args.budget:
        checks=budget_checks(json.loads(args.budget.read_text())['runs'])
        print(json.dumps(checks,sort_keys=True))
        sys.exit(0 if all(checks.values()) else 1)
    key=os.environ.get('TYPESAFE_API_KEY')
    if not key:
        sys.exit('TYPESAFE_API_KEY is required')
    binary=ROOT/'target/release/manuvra'
    subprocess.run(['cargo','build','--release','--locked','--bin','manuvra'],cwd=ROOT,check=True)
    matrix=ROOT/'.work/live/scroll'/(time.strftime('%Y%m%d-%H%M%S')+f'-{os.getpid()}')
    matrix.mkdir(parents=True)
    server=FixtureServer()
    threading.Thread(target=server.serve_forever,daemon=True).start()
    rows=[]
    try:
        for journey in JOURNEYS:
            for iteration in range(1,ITERATIONS+1):
                try:
                    row=run_case(binary,server,matrix,journey,iteration)
                except Exception as error:
                    row={'journey':journey,'iteration':iteration,'classification':'failed','violations':[type(error).__name__+': '+str(error)]}
                rows.append(row)
                report(matrix,binary,rows,key)
                print(f'{journey}-{iteration}: {row["classification"]} {row["violations"]}',flush=True)
    finally:
        server.shutdown();server.server_close()
    data=report(matrix,binary,rows,key)
    print(f'report: {matrix/"report.json"}',flush=True)
    if not data['threshold_met']:
        sys.exit('scroll matrix threshold not met: '+', '.join(k for k,v in data['checks'].items() if not v))
    print('scroll matrix threshold met',flush=True)

if __name__=='__main__':
    main()
