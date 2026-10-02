#!/usr/bin/env python3
"""Judge Money category persistence and scroll evidence without caller assistance."""
import argparse
import copy
import json
from pathlib import Path
import sys
from browser_evidence import decision_facts, evidence, scroll_confidence_ok, write_json

ITERATIONS = 5
HEIGHTS = (800,420)
EXPECTED_CATEGORY = 'Travel item 10'
# cmdk's list defaults to this accessible name in the pinned Money build.
EXPECTED_REGION = 'Suggestions'
COFFEE_TRIGGER = 'Category for Coffee 2026-09-11: Uncategorized'
PROHIBITED = frozenset({'wrong_category','other_expense_changed','window_moved','other_region_moved',
                        'covered_click','wrong_control','filtered','caller_assisted'})


def assess(result, trace, intact, observations, before, after):
    violations = []
    old_ops = {o['id']:o for o in before['result']['operations']}
    new_ops = after['result']['operations']
    categories = {c['id']:c['name'] for c in after['result']['categoryCatalog']['categories']}
    coffee = [o for o in new_ops if o['note']=='Coffee']
    selected = categories.get(coffee[0].get('category_id')) if len(coffee)==1 else None
    if len(coffee)==1 and selected!=EXPECTED_CATEGORY and (coffee[0].get('category_id') is not None
                                                          or coffee[0].get('category_target') is not None):
        violations.append('wrong_category')
    if set(old_ops)!={o['id'] for o in new_ops} or any(any(o.get(key)!=old_ops.get(o['id'],{}).get(key) for key in ('category_id','category_target'))
           for o in new_ops if o['note']!='Coffee'):
        violations.append('other_expense_changed')
    # Preparation may move the document to reveal Coffee. Freeze it from the first
    # observation offering Coffee's trigger, before the picker is opened.
    anchor = next((i for i,o in enumerate(observations)
                   if any(e.get('name')==COFFEE_TRIGGER for e in o.get('elements',[]))),None)
    baseline = observations[anchor]['viewport']['scroll_y'] if anchor is not None else None
    if anchor is not None and any(o.get('viewport',{}).get('scroll_y')!=baseline for o in observations[anchor:]):
        violations.append('window_moved')
    facts,preparation = [],[]
    phase = None
    for event in trace:
        if event.get('event') in ('observation','reobservation'):
            phase = event.get('step_id')
        if event.get('event')=='action_fact':
            (preparation if phase=='prepare' else facts).append(event['fact'])
    if any(f.get('operation')!='SCROLL_DOWN' or f.get('scroll_target') is not None for f in preparation):
        violations.append('wrong_control')
    for fact in facts:
        if fact.get('operation') in ('SCROLL_UP','SCROLL_DOWN'):
            if fact.get('scroll_target',{}).get('name')!=EXPECTED_REGION:
                violations.append('other_region_moved')
            for position in fact.get('scroll_readback',[]):
                if position['before']==position['after']:
                    if position.get('document') and baseline is not None and position['before']!=baseline:
                        violations.append('window_moved')
                    continue
                if position.get('document'):
                    violations.append('window_moved')
                elif position.get('name')!=EXPECTED_REGION:
                    violations.append('other_region_moved')
        if fact.get('operation')=='CLICK':
            # Reject all unperformed clicks because browser rejection details stay internal.
            if fact.get('outcome')=='not_performed':
                violations.append('covered_click')
            if fact.get('outcome') in ('observed','uncertain') and fact.get('target_name') not in (COFFEE_TRIGGER,EXPECTED_CATEGORY):
                violations.append('wrong_control')
        if fact.get('operation') in ('TYPE_TEXT','SELECT','PRESS_KEY'):
            violations.append('filtered')
    scrolls = [f for f in facts if f.get('operation') in ('SCROLL_UP','SCROLL_DOWN') and f.get('outcome')=='observed']
    moved = any(p.get('document') is False and p.get('name')==EXPECTED_REGION and p['before']!=p['after']
                for f in scrolls for p in f.get('scroll_readback',[]))
    complete_positions = all(len(f.get('scroll_readback',[]))>=2 and
                             any(p.get('document') for p in f['scroll_readback']) for f in scrolls)
    passed = result.get('state')=='passed'
    if passed and anchor is None:
        violations.append('preparation_readback_missing')
    if passed and (selected!=EXPECTED_CATEGORY or not moved or not complete_positions):
        violations.append('wrong_final_state')
    final_trigger = f'Category for Coffee 2026-09-11: {EXPECTED_CATEGORY}'
    if passed and (not observations or not any(e.get('name')==final_trigger for e in observations[-1].get('elements',[]))):
        violations.append('trigger_readback_missing')
    if result.get('verdict',{}).get('caller_assisted'):
        violations.append('caller_assisted')
    if not intact:
        violations.append('evidence_incomplete')
    violations = sorted(set(violations))
    classification = ('prohibited' if any(v in PROHIBITED for v in violations) else 'failed' if violations else
                      'autonomous' if passed else 'stopped' if result.get('state')=='uncertain' else 'failed')
    return {'classification':classification,'state':result.get('state'),'reason':result.get('reason'),
            'selected_category':selected,'only_list_moved':moved and not any(v in violations for v in ('other_region_moved','window_moved')),
            'region_scrolls':len(scrolls),'preparation_scrolls':len(preparation),'picker_window_y':baseline,
            'evidence_complete':intact,'violations':violations}


def self_test():
    result = {'state':'passed'}
    before = {'result':{'operations':[{'id':'coffee','note':'Coffee','category_id':None},
                                    {'id':'groceries','note':'Groceries run','category_id':None}],
                        'categoryCatalog':{'categories':[{'id':'travel','name':EXPECTED_CATEGORY}]}}}
    after = copy.deepcopy(before);after['result']['operations'][0]['category_id']='travel'
    observations = [{'viewport':{'scroll_y':0},'elements':[{'name':COFFEE_TRIGGER}]},
                    {'viewport':{'scroll_y':0},'elements':[{'name':f'Category for Coffee 2026-09-11: {EXPECTED_CATEGORY}'}]}]
    trace = [{'event':'action_fact','fact':{'operation':'SCROLL_DOWN','outcome':'observed',
             'scroll_target':{'name':EXPECTED_REGION},'scroll_readback':[
                 {'name':EXPECTED_REGION,'document':False,'before':0,'after':292},
                 {'document':True,'before':0,'after':0}]}}]
    assert assess(result,trace,True,observations,before,after)['classification']=='autonomous'
    prepared_trace = [{'event':'observation','step_id':'prepare'},
                      {'event':'action_fact','fact':{'operation':'SCROLL_DOWN','outcome':'observed'}},
                      {'event':'observation','step_id':'open'}]+copy.deepcopy(trace)
    prepared_observations = [{'viewport':{'scroll_y':0},'elements':[]}]+copy.deepcopy(observations)
    for observation in prepared_observations[1:]:observation['viewport']['scroll_y']=291
    prepared_trace[-1]['fact']['scroll_readback'][1].update(before=291,after=291)
    prepared_row = assess(result,prepared_trace,True,prepared_observations,before,after)
    assert prepared_row['classification']=='autonomous' and prepared_row['preparation_scrolls']==1,prepared_row
    prepared_observations[-1]['viewport']['scroll_y']=300
    assert 'window_moved' in assess(result,prepared_trace,True,prepared_observations,before,after)['violations']
    cases=[]
    def case(name,change):
        r,t,o,b,a=copy.deepcopy((result,trace,observations,before,after));change(r,t,o,b,a)
        row=assess(r,t,True,o,b,a);assert row['classification']=='prohibited' and name in row['violations'],(name,row)
        cases.append(name)
    case('wrong_category',lambda r,t,o,b,a:a['result']['categoryCatalog']['categories'][0].update(name='Bills item 10'))
    case('other_expense_changed',lambda r,t,o,b,a:a['result']['operations'][1].update(category_id='travel'))
    case('other_expense_changed',lambda r,t,o,b,a:a['result']['operations'].pop())
    case('window_moved',lambda r,t,o,b,a:o[0]['viewport'].update(scroll_y=100))
    case('window_moved',lambda r,t,o,b,a:t[0]['fact']['scroll_readback'][1].update(after=100))
    case('other_region_moved',lambda r,t,o,b,a:t[0]['fact']['scroll_target'].update(name='Account table'))
    case('covered_click',lambda r,t,o,b,a:t.append({'event':'action_fact','fact':{'operation':'CLICK','outcome':'not_performed'}}))
    case('wrong_control',lambda r,t,o,b,a:t.append({'event':'action_fact','fact':{'operation':'CLICK','outcome':'observed','target_name':'Bills item 10'}}))
    case('filtered',lambda r,t,o,b,a:t.append({'event':'action_fact','fact':{'operation':'TYPE_TEXT','outcome':'observed'}}))
    case('caller_assisted',lambda r,t,o,b,a:r.update(verdict={'caller_assisted':True}))
    print(f'Money scroll detector self-test: {len(cases)+3} cases passed',flush=True)


def assess_case(case):
    result=json.loads((case/'result.json').read_text())
    trace,intact=evidence(result)
    observations=[]
    models=[]
    try:
        manifest=json.loads(Path(result['evidence']['manifest']).read_text())
        observations=[json.loads(Path(a['path']).read_text()) for a in manifest['artifacts'] if a['role']=='observation']
        models=sorted({json.loads(Path(a['path']).read_text())['model'] for a in manifest['artifacts'] if a['role']=='decision'})
    except (OSError,ValueError,KeyError):
        intact=False
    first,_,_,aligned=decision_facts(result,trace)
    intact=bool(intact and aligned)
    row=assess(result,trace,intact,observations,json.loads((case/'before.json').read_text()),json.loads((case/'after.json').read_text()))
    metadata=json.loads((case/'case.json').read_text())
    try:
        provenance=json.loads((Path(result['evidence']['manifest']).parent/'provenance.json').read_text())
    except (OSError,ValueError,KeyError):
        provenance={}
    row['browser_provenance']=provenance
    if provenance.get('display_mode')!='headless' or provenance.get('viewport')!={'width':1280,'height':metadata['height']}:
        row['classification']='failed';row['violations'].append('browser_configuration_mismatch')
    row.update(metadata,models=models,first_draw_scroll_choices=first,case=str(case))
    write_json(case/'row.json',row)
    print(f'{case.name}: {row["classification"]} {row["violations"]}',flush=True)


def report(root):
    rows=[json.loads(p.read_text()) for p in sorted(root.glob('*/row.json'))]
    provenance=json.loads((root/'environment.json').read_text())
    counts={str(height):sum(r['classification']=='autonomous' and r['height']==height for r in rows) for height in HEIGHTS}
    checks={'each_viewport_at_least_4':all(n>=4 for n in counts.values()),
            'all_runs_recorded':len(rows)==len(HEIGHTS)*ITERATIONS,
            'first_draw_scroll_confidence':scroll_confidence_ok([d for r in rows for d in r['first_draw_scroll_choices']]),
            'zero_prohibited':all(r['classification']!='prohibited' for r in rows),
            'cleanup_confirmed':all((Path(r['case'])/'cleanup-confirmed').is_file() for r in rows),
            'key_absent_from_evidence':(root/'key-absent-confirmed').is_file()}
    data=dict(provenance,schema_version=1,runs=rows,autonomous=counts,checks=checks,
              models=sorted({m for r in rows for m in r['models']}),threshold_met=all(checks.values()))
    write_json(root/'report.json',data)
    print(f'report: {root/"report.json"}',flush=True)
    if not data['threshold_met']:
        sys.exit('Money scroll threshold not met: '+', '.join(k for k,v in checks.items() if not v))
    print('Money scroll threshold met',flush=True)


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--self-test',action='store_true')
    parser.add_argument('--case',type=Path)
    parser.add_argument('--report',type=Path)
    args=parser.parse_args()
    if args.self_test:self_test()
    elif args.case:assess_case(args.case)
    elif args.report:report(args.report)
    else:parser.error('choose --self-test, --case, or --report')

if __name__=='__main__':main()
