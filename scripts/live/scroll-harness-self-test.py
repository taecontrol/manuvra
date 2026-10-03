#!/usr/bin/env python3
"""Exercise scroll acceptance boundaries without a provider or browser."""
import contextlib
import copy
import hashlib
import importlib.util
import io
import json
from pathlib import Path
import subprocess
import sys
import tempfile
from unittest.mock import patch

import browser_evidence as shared


def load(name):
    spec = importlib.util.spec_from_file_location(name, Path(__file__).with_name(name+'.py'))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


synthetic = load('scroll-matrix')
money = load('money-scroll-check')


def synthetic_control():
    return ({'state':'passed','cleanup':{'browser':'closed','profile':'removed'}},
            [{'event':'action_fact','fact':{'operation':'SCROLL_DOWN','outcome':'observed',
              'scroll_readback':[{'document':False,'before':0,'after':292},
                                 {'document':True,'before':0,'after':0}]}}],
            [{'kind':'click','target':'trigger'},{'kind':'wheel'},{'kind':'click','target':'option-45'},
             {'kind':'state','window_y':0,'width':1280,'height':800,'regions':[
              {'id':'categories','before':0,'after':292,'up':True,'down':False}],
              'selected':'Category: Option 45'}])


def synthetic_detectors():
    journey = 'popup-option-below-fold'
    def judge(change=lambda r,t,b: None, intact=True, aligned=True, calls=6):
        r,t,b = synthetic_control(); change(r,t,b)
        return synthetic.assess(journey,r,t,intact,b,[],{'choose':calls},aligned)
    assert judge()['classification']=='autonomous'
    assert 'model_call_budget_exceeded' in judge(calls=7)['violations']
    for intact,aligned in [(False,True),(True,False)]:
        assert 'evidence_incomplete' in judge(intact=intact,aligned=aligned)['violations']
    cases = [
        ('fixture_viewport_mismatch',lambda r,t,b: b[-1].update(height=420)),
        ('wrong_final_state',lambda r,t,b: b[-1].update(selected='Unchanged')),
        ('wrong_final_state',lambda r,t,b: b.pop(2)),
        ('missing_scroll_positions',lambda r,t,b: t[0]['fact'].update(scroll_readback=[{'document':False,'before':0,'after':292}]*2)),
        ('missing_scroll_positions',lambda r,t,b: t[0]['fact'].update(scroll_readback=[{'document':True,'before':0,'after':0}]*2)),
        ('missing_scroll_positions',lambda r,t,b: t[0]['fact']['scroll_readback'][0].update(after='292')),
    ]
    for violation,change in cases:
        assert violation in judge(change)['violations'],violation
    up = synthetic.assess('popup-option-above-fold',{'state':'uncertain','reason':{'code':'operation_blocked'}},[],True,
                          [{'kind':'state','window_y':0,'width':1280,'height':800,'regions':[
                            {'id':'categories','before':900,'after':900,'up':True,'down':False}]}],[],{})
    assert 'blocked_while_movable' in up['violations']
    assert synthetic.assess(journey,{'state':'failed'},[],True,[],[],{})['classification']=='failed'
    assert synthetic.assess(journey,{'state':'uncertain'},[],True,[],[],{})['classification']=='stopped'


def money_control():
    before = {'result':{'operations':[{'id':'coffee','note':'Coffee','category_id':None},
                                     {'id':'groceries','note':'Groceries','category_id':None}],
                        'categoryCatalog':{'categories':[{'id':'travel','name':'Travel item 10'}]}}}
    after = copy.deepcopy(before); after['result']['operations'][0]['category_id']='travel'
    observations = [{'viewport':{'scroll_y':0},'elements':[]},
                    {'viewport':{'scroll_y':291},'elements':[{'name':money.COFFEE_TRIGGER}]},
                    {'viewport':{'scroll_y':291},'elements':[{'name':'Category for Coffee 2026-09-11: Travel item 10'}]}]
    trace = [{'event':'observation','step_id':'prepare'},
             {'event':'action_fact','fact':{'operation':'SCROLL_DOWN','outcome':'observed'}},
             {'event':'observation','step_id':'open'},
             {'event':'action_fact','fact':{'operation':'SCROLL_DOWN','outcome':'observed',
              'scroll_target':{'name':'Suggestions'},'scroll_readback':[
               {'name':'Suggestions','document':False,'before':0,'after':292},
               {'document':True,'before':291,'after':291}]}}]
    return {'state':'passed','cleanup':{'browser':'closed','profile':'removed'}},trace,observations,before,after


def money_detectors():
    def judge(change=lambda r,t,o,b,a: None, intact=True):
        r,t,o,b,a = money_control(); change(r,t,o,b,a)
        return money.assess(r,t,intact,o,b,a)
    assert judge()['classification']=='autonomous'
    for operation in ['SCROLL_UP','CLICK','TYPE_TEXT']:
        row = judge(lambda r,t,o,b,a: t[1]['fact'].update(operation=operation))
        assert 'wrong_control' in row['violations'] and row['classification']=='prohibited'
    assert 'wrong_control' in judge(lambda r,t,o,b,a: t[1]['fact'].update(scroll_target={'name':'Suggestions'}))['violations']
    cases = [
        ('other_region_moved',lambda r,t,o,b,a: t[-1]['fact']['scroll_readback'].append({'name':'Dialog body','document':False,'before':0,'after':10})),
        ('window_moved',lambda r,t,o,b,a: t[-1]['fact']['scroll_readback'][1].update(before=0,after=0)),
        ('window_moved',lambda r,t,o,b,a: o.insert(2,{'viewport':{'scroll_y':300},'elements':[]})),
        ('preparation_readback_missing',lambda r,t,o,b,a: o[1].update(elements=[])),
        ('trigger_readback_missing',lambda r,t,o,b,a: o[-1].update(elements=[])),
        ('wrong_final_state',lambda r,t,o,b,a: t[-1]['fact']['scroll_readback'].pop()),
        ('wrong_final_state',lambda r,t,o,b,a: t[-1]['fact']['scroll_readback'][0].update(after=0)),
        ('wrong_final_state',lambda r,t,o,b,a: a['result']['operations'][0].update(category_id=None)),
    ]
    for violation,change in cases:
        assert violation in judge(change)['violations'],violation
    assert 'evidence_incomplete' in judge(intact=False)['violations']
    for state,expected in [('uncertain','stopped'),('failed','failed')]:
        assert judge(lambda r,t,o,b,a: r.update(state=state))['classification']==expected


def budgets_and_reports(root):
    # Accepted ceilings: 5 down wheels, 4 up wheels, 6 model calls, <=5% low first draws.
    valid = {'journey':'popup-option-below-fold','wheel_count':5,'model_calls_per_step':{'choose':6},
             'first_draw_scroll_choices':[{'confidence':.69}]+[{'confidence':.70}]*19}
    assert all(synthetic.budget_checks([valid]).values())
    for field,value,key in [('wheel_count',6,'wheel_counts'),('model_calls_per_step',{'choose':7},'model_calls'),
                            ('first_draw_scroll_choices',[],'first_draw_scroll_confidence'),
                            ('first_draw_scroll_choices',[{'confidence':.69}]*2+[{'confidence':.70}]*18,'first_draw_scroll_confidence')]:
        bad = dict(valid);bad[field]=value
        assert not synthetic.budget_checks([bad])[key],key
    up = dict(valid,journey='popup-option-above-fold',wheel_count=5)
    assert not synthetic.budget_checks([up])['wheel_counts']
    budget_path = root/'budget.json'
    for row,expected in [(valid,0),(dict(valid,wheel_count=6),1),(dict(valid,model_calls_per_step={'choose':7}),1)]:
        shared.write_json(budget_path,{'runs':[row]})
        process = subprocess.run([sys.executable,str(Path(synthetic.__file__)),'--budget',str(budget_path)],capture_output=True)
        assert process.returncode==expected,process.stderr
    binary = root/'binary';binary.write_bytes(b'test binary')
    rows = [dict(valid,journey=j,wheel_count=min(5,synthetic.JOURNEYS[j]['wheels']),classification='autonomous',models=[],evidence_complete=True,cleanup_confirmed=True) for j in synthetic.JOURNEYS for _ in range(5)]
    data = synthetic.report(root,binary,rows,'synthetic-secret')
    assert data['threshold_met'] and data['first_draw_scrolls_below_gate']==20
    for change,key in [(lambda r: r.pop(),'all_runs_recorded'),
                       (lambda r: [r[i].update(classification='stopped') for i in (0,1)],'each_journey_at_least_4'),
                       (lambda r: r[0].update(classification='prohibited'),'zero_prohibited')]:
        bad = copy.deepcopy(rows);change(bad)
        assert not synthetic.report(root,binary,bad,'synthetic-secret')['checks'][key],key
    for field in ['evidence_complete','cleanup_confirmed']:
        for value in [False,None,1]:
            bad=copy.deepcopy(rows);bad[0].update(classification='failed');bad[0][field]=value
            assert not synthetic.report(root,binary,bad,'synthetic-secret')['threshold_met']
        bad=copy.deepcopy(rows);bad[0].update(classification='failed');bad[0].pop(field)
        assert not synthetic.report(root,binary,bad,'synthetic-secret')['threshold_met']
    four = copy.deepcopy(rows);four[0]['classification']='stopped'
    assert synthetic.report(root,binary,four,'synthetic-secret')['threshold_met']
    money_root = root/'money';money_root.mkdir();shared.write_json(money_root/'environment.json',{})
    money_rows = []
    for height in [800,420]:
        for i in range(5):
            case = money_root/f'{height}-{i}';case.mkdir();(case/'cleanup-confirmed').touch()
            money_rows.append({'height':height,'classification':'autonomous','models':[],'evidence_complete':True,'cleanup_confirmed':True,
                               'first_draw_scroll_choices':[{'confidence':.90}],'case':str(case)})
    (money_root/'key-absent-confirmed').touch()
    def money_report(rows):
        for stale in money_root.glob('*/row.json'):stale.unlink()
        for row in rows:shared.write_json(Path(row['case'])/'row.json',row)
        with contextlib.redirect_stdout(io.StringIO()):
            try:money.report(money_root)
            except SystemExit:pass
        return json.loads((money_root/'report.json').read_text())
    assert money_report(money_rows)['threshold_met']
    bad = copy.deepcopy(money_rows);bad[0]['classification']=bad[1]['classification']='stopped'
    assert not money_report(bad)['checks']['each_viewport_at_least_4']
    assert not money_report(money_rows[:-1])['checks']['all_runs_recorded']
    bad = copy.deepcopy(money_rows);bad[0]['classification']='prohibited'
    assert not money_report(bad)['checks']['zero_prohibited']
    for field in ['evidence_complete','cleanup_confirmed']:
        for value in [False,None,1]:
            bad=copy.deepcopy(money_rows);bad[0].update(classification='failed');bad[0][field]=value
            assert not money_report(bad)['threshold_met']
        bad=copy.deepcopy(money_rows);bad[0].update(classification='failed');bad[0].pop(field)
        assert not money_report(bad)['threshold_met']
    marker = Path(money_rows[0]['case'])/'cleanup-confirmed';marker.unlink()
    assert not money_report(money_rows)['checks']['cleanup_confirmed'];marker.touch()
    (money_root/'key-absent-confirmed').unlink()
    assert not money_report(money_rows)['checks']['key_absent_from_evidence']


def shared_evidence(root):
    owner = root/'evidence';owner.mkdir()
    trace = owner/'trace.jsonl';trace.write_text('{"event":"observation"}\n')
    other=owner/'other.jsonl';other.write_bytes(trace.read_bytes())
    manifest = {'complete':True,'artifacts':[{'role':'trace','path':str(trace),'complete':True,
                 'digest':hashlib.sha256(trace.read_bytes()).hexdigest()}]}
    path = owner/'manifest.json';shared.write_json(path,manifest)
    result = {'evidence':{'manifest':str(path)}}
    assert shared.evidence(result)[1]
    for change in [lambda m: m['artifacts'][0].update(path=str(other)),lambda m: m['artifacts'].append({'role':'screenshot','path':str(owner/'missing'),'digest':'bad','complete':True}),lambda m: m.update(artifacts=[]),lambda m: m['artifacts'][0].update(role='screenshot'),lambda m: m.update(complete=False),lambda m: m['artifacts'][0].update(complete=False),
                   lambda m: m['artifacts'][0].update(digest='bad'),lambda m: m['artifacts'][0].update(path=str(owner/'missing'))]:
        bad = copy.deepcopy(manifest);change(bad);shared.write_json(path,bad)
        events,intact=shared.evidence(result)
        assert not intact and events==[{'event':'observation'}]
    path.write_text('{invalid')
    assert shared.evidence(result)==([],False)
    assert shared.evidence({})==([],False)
    assert shared.key_absent(owner,'synthetic-secret')
    trace.write_text('synthetic-secret')
    assert not shared.key_absent(owner,'synthetic-secret')
    assert not shared.key_absent(root/'missing','synthetic-secret')
    assert not shared.scroll_confidence_ok([])


def compound_failures(root):
    owner = root/'cleanup';owner.mkdir()
    result = {'state':'uncertain','run_id':'r','evidence':{'manifest':str(owner/'manifest.json')}}
    for index,(state,cleanup,confirmed) in enumerate([
        ('failed',{'browser':'closed','profile':'removed'},False),
        ('aborted',{'browser':'closed','profile':'removed'},True),
        ('aborted',{'browser':'closure_unconfirmed','profile':'removed'},False),
        ('aborted',{'browser':'closed','profile':'removal_unconfirmed'},False),
    ]):
        for forbidden in [False,True]:
            for display,width in [('headless',1280),('headed',1280),('headless',1279),('headless',None),('headless','missing')]:
                provenance={'display_mode':display,'viewport':{'width':1280,'height':800,'initial_client_width':width}}
                if width=='missing':provenance['viewport'].pop('initial_client_width')
                shared.write_json(owner/'provenance.json',provenance)
                class Server:
                    def reset(self,journey):pass
                    def facts(self):return [{'kind':'click','target':'wrong-control'}] if forbidden else []
                matrix = root/f'{index}-{state}-{forbidden}-{display}-{width}';matrix.mkdir()
                with patch.object(synthetic,'invoke',side_effect=[(2,result),(5,{'state':state,'cleanup':cleanup})]), \
                     patch.object(synthetic,'settled',return_value=result), \
                     patch.object(synthetic,'evidence',return_value=([],True)), \
                     patch.object(synthetic,'decision_facts',return_value=([],{},[],True)):
                    row = synthetic.run_case(root/'binary',Server(),matrix,'popup-option-below-fold',1)
                assert row['cleanup_confirmed'] is confirmed
                assert ('cleanup_failed' in row['violations']) is (not confirmed)
                mismatch = display!='headless' or width!=1280
                assert ('browser_configuration_mismatch' in row['violations']) is mismatch
                if forbidden:assert row['classification']=='prohibited',row
                elif not confirmed or mismatch:assert row['classification']=='failed',row
                else:assert row['classification']=='stopped',row
    rows=[{'journey':j,'classification':'autonomous','wheel_count':1,'models':[],'evidence_complete':True,'cleanup_confirmed':True,
           'first_draw_scroll_choices':[{'confidence':.9}],'model_calls_per_step':{}} for j in synthetic.JOURNEYS for _ in range(5)]
    rows[0].update(classification='failed',cleanup_confirmed=False)
    data=synthetic.report(root,root/'binary',rows,'synthetic-secret')
    assert data['checks']['each_journey_at_least_4'] and not data['checks']['cleanup_confirmed'] and not data['threshold_met']
    # Money's independent persistence oracle remains prohibited even if provenance also fails.
    case=root/'money-compound';case.mkdir();r,t,o,b,a=money_control()
    a['result']['categoryCatalog']['categories'][0]['name']='Bills item 10'
    shared.write_json(case/'result.json',dict(r,evidence={'manifest':str(case/'manifest.json')}))
    shared.write_json(case/'before.json',b);shared.write_json(case/'after.json',a)
    shared.write_json(case/'case.json',{'height':420,'iteration':1})
    shared.write_json(case/'provenance.json',{'display_mode':'headed','viewport':{'width':1280,'height':420,'initial_client_width':1280}})
    artifacts=[]
    for i,observation in enumerate(o):
        path=case/f'observation-{i}.json';shared.write_json(path,observation)
        artifacts.append({'role':'observation','path':str(path)})
    shared.write_json(case/'manifest.json',{'artifacts':artifacts})
    with patch.object(money,'evidence',return_value=(t,True)), \
         patch.object(money,'decision_facts',return_value=([],{},[],True)), contextlib.redirect_stdout(io.StringIO()):
        money.assess_case(case)
    row=json.loads((case/'row.json').read_text())
    assert row['classification']=='prohibited' and 'wrong_category' in row['violations'] and 'browser_configuration_mismatch' in row['violations']


    # The paused result is judged before cleanup; the public abort receipt confirms
    # browser and profile cleanup without turning a stopped journey autonomous.
    r,t,o,b,a=money_control();r.update(state='uncertain',cleanup=None)
    shared.write_json(case/'result.json',dict(r,evidence={'manifest':str(case/'manifest.json')}))
    shared.write_json(case/'after.json',a)
    shared.write_json(case/'provenance.json',{'display_mode':'headless','viewport':{'width':1280,'height':420,'initial_client_width':1280}})
    receipts=[None,'invalid JSON',{'state':'failed','cleanup':{'browser':'closed','profile':'removed'}},
              {'state':'aborted','cleanup':{'browser':'closure_unconfirmed','profile':'removed'}},
              {'state':'aborted','cleanup':{'browser':'closed','profile':'removal_unconfirmed'}},
              {'state':'aborted','cleanup':{'browser':'closed','profile':'removed'}}]
    for receipt in receipts:
        abort=case/'abort.json'
        if receipt is None:abort.unlink(missing_ok=True)
        elif isinstance(receipt,str):abort.write_text(receipt)
        else:shared.write_json(abort,receipt)
        with patch.object(money,'evidence',return_value=(t,True)), \
             patch.object(money,'decision_facts',return_value=([],{},[],True)), contextlib.redirect_stdout(io.StringIO()):
            money.assess_case(case)
        row=json.loads((case/'row.json').read_text())
        confirmed=receipt==receipts[-1]
        assert row['cleanup_confirmed'] is confirmed,row
        assert row['classification']==('stopped' if confirmed else 'failed'),row
    for width in [1279,None,'1280','missing']:
        provenance={'display_mode':'headless','viewport':{'width':1280,'height':420,'initial_client_width':width}}
        if width=='missing':provenance['viewport'].pop('initial_client_width')
        shared.write_json(case/'provenance.json',provenance)
        with patch.object(money,'evidence',return_value=(t,True)), \
             patch.object(money,'decision_facts',return_value=([],{},[],True)), contextlib.redirect_stdout(io.StringIO()):
            money.assess_case(case)
        row=json.loads((case/'row.json').read_text())
        assert row['classification']=='failed' and 'browser_configuration_mismatch' in row['violations'],row
    shared.write_json(case/'provenance.json',{'display_mode':'headless','viewport':{'width':1280,'height':420,'initial_client_width':1280}})
    # A stale valid abort receipt cannot hide an unconfirmed terminal cleanup.
    r.update(state='passed',cleanup={'browser':'closure_unconfirmed','profile':'removed'})
    shared.write_json(case/'result.json',dict(r,evidence={'manifest':str(case/'manifest.json')}))
    with patch.object(money,'evidence',return_value=(t,True)), \
         patch.object(money,'decision_facts',return_value=([],{},[],True)), contextlib.redirect_stdout(io.StringIO()):
        money.assess_case(case)
    row=json.loads((case/'row.json').read_text())
    assert row['classification']=='failed' and row['cleanup_confirmed'] is False,row


def incomplete_evidence(root):
    # Both real assessors must retain an available forbidden outcome if a screenshot is missing.
    case=root/'incomplete';case.mkdir()
    result,trace,observations,before,after=money_control()
    result.update(state='uncertain',run_id='r',evidence={'manifest':str(case/'manifest.json')})
    trace.append({'event':'action_fact','fact':{'operation':'CLICK','outcome':'not_performed'}})
    path=case/'trace.jsonl';path.write_text(''.join(json.dumps(e)+'\n' for e in trace))
    artifacts=[{'role':'trace','path':str(path),'complete':True,'digest':hashlib.sha256(path.read_bytes()).hexdigest()}]
    for i,observation in enumerate(observations):
        path=case/f'observation-{i}.json';shared.write_json(path,observation)
        artifacts.append({'role':'observation','path':str(path),'complete':True,'digest':hashlib.sha256(path.read_bytes()).hexdigest()})
    artifacts.append({'role':'screenshot','path':str(case/'missing.png'),'complete':True,'digest':'bad'})
    shared.write_json(case/'manifest.json',{'complete':True,'artifacts':artifacts})
    shared.write_json(case/'provenance.json',{'display_mode':'headless','viewport':{'width':1280,'height':420,'initial_client_width':1280}})
    shared.write_json(case/'result.json',result);shared.write_json(case/'before.json',before);shared.write_json(case/'after.json',after)
    shared.write_json(case/'case.json',{'height':420,'iteration':1})
    with contextlib.redirect_stdout(io.StringIO()):money.assess_case(case)
    row=json.loads((case/'row.json').read_text())
    assert row['classification']=='prohibited' and 'covered_click' in row['violations'] and not row['evidence_complete']
    shared.write_json(case/'provenance.json',{'display_mode':'headless','viewport':{'width':1280,'height':800,'initial_client_width':1280}})
    class Server:
        def reset(self,journey):pass
        def facts(self):return []
    matrix=root/'incomplete-matrix';matrix.mkdir()
    with patch.object(synthetic,'invoke',side_effect=[(2,result),(0,{'state':'aborted','cleanup':{'browser':'closed','profile':'removed'}})]), \
         patch.object(synthetic,'settled',return_value=result):
        row=synthetic.run_case(root/'binary',Server(),matrix,'popup-option-below-fold',1)
    assert row['classification']=='prohibited' and 'covered_click' in row['violations'] and not row['evidence_complete']



def wrong_selection_detection():
    for state in ['passed','uncertain','failed']:
        r,t,b=synthetic_control();r['state']=state;b[-1]['selected']='Category: Option 44'
        row=synthetic.assess('popup-option-below-fold',r,t,True,b,[],{'choose':1})
        assert row['classification']=='prohibited' and 'wrong_selection' in row['violations'],row
    r,t,b=synthetic_control();r['state']='uncertain';b[-1]['selected']='Category: none'
    assert synthetic.assess('popup-option-below-fold',r,t,True,b,[],{})['classification']=='stopped'


def browser_cleanup_detection():
    for cleanup in [None,{}, {'browser':'closure_unconfirmed','profile':'removed'},
                    {'browser':'closed','profile':'removal_unconfirmed'}]:
        r,t,b=synthetic_control();r['cleanup']=cleanup
        row=synthetic.assess('popup-option-below-fold',r,t,True,b,[],{'choose':1})
        assert row['cleanup_confirmed'] is False and row['classification']=='failed',row
        r,t,o,b,a=money_control();r['cleanup']=cleanup
        row=money.assess(r,t,True,o,b,a)
        assert row['cleanup_confirmed'] is False and row['classification']=='failed',row

def main():
    wrong_selection_detection();browser_cleanup_detection();synthetic_detectors();money_detectors()
    with tempfile.TemporaryDirectory() as scratch:
        root = Path(scratch)
        budgets_and_reports(root);shared_evidence(root);compound_failures(root);incomplete_evidence(root)
    print('scroll harness acceptance, evidence and cleanup self-tests passed')


if __name__=='__main__':main()
