"""ProjectionRangeTests ported 1:1 from the B1 offline harness (test_full_engine_500_offline_v2.py)."""
import copy
import unittest

from fragment_projection import _known_turn_ranges, _marker_role, known_turn_ranges, v2_projection_proven


class ProjectionRangeTests(unittest.TestCase):
    def test_same_role_turn_swap_empty_and_overlap_are_rejected(self):
        content='user: first\nuser: second'
        session={'turns':[{'role':'user','content':'first'},{'role':'user','content':'second'}]}
        source={'source_id':'src','content':content,'conversation_date':'2024-01-01','fragment_ids':['f0','f1']}
        def fragment(ident,start,end,index):
            return {'id':ident,'content':content.encode()[start:end].decode(), 'fragment':{
                'byte_start':start,'byte_end':end,'conversation_date':'2024-01-01',
                'role':'user','source_id':'src','turn_index':index}}
        first=len('user: first\n'.encode())
        proof={'request':{'source':source},'response':{'schema_version':2,
            'evidence':'fresh_immutable_ingest_projections_v2','fragment_projections':[
                fragment('f0',0,first,0),fragment('f1',first,len(content.encode()),1)]}}
        self.assertTrue(v2_projection_proven(proof,session))
        changed=copy.deepcopy(proof);changed['response']['fragment_projections'][0]['fragment']['turn_index']=1
        self.assertFalse(v2_projection_proven(changed,session))
        changed=copy.deepcopy(proof);changed['response']['fragment_projections'][0]['content']=''
        changed['response']['fragment_projections'][0]['fragment']['byte_end']=0
        self.assertFalse(v2_projection_proven(changed,session))
        changed=copy.deepcopy(proof);changed['response']['fragment_projections'][1]=fragment('f1',1,first,0)
        self.assertFalse(v2_projection_proven(changed,session))
        # Sanitizer may trim boundaries; gaps do not invalidate exact slices.
        changed=copy.deepcopy(proof);changed['response']['fragment_projections'][0]=fragment('f0',0,first-1,0)
        self.assertTrue(v2_projection_proven(changed,session))

    def test_embedded_markers_use_rust_turn_indices_and_single_turn_is_unlabelled(self):
        content='user: first\nassistant: inserted\nuser: second'
        session={'turns':[{'role':'user','content':'first\nassistant: inserted'},
                          {'role':'user','content':'second'}]}
        proof={'request':{'source':{'source_id':'src','content':content,'fragment_ids':[]}},
               'response':{'schema_version':2,'evidence':'fresh_immutable_ingest_projections_v2','fragment_projections':[]}}
        first=len('user: first\n'.encode()); second=len('user: first\nassistant: inserted\n'.encode())
        proof['request']['source'].update(fragment_ids=['f0','f1','f2'],conversation_date=None)
        def fragment(ident,start,end,role,index):
            return {'id':ident,'content':content.encode()[start:end].decode(),'fragment':{'byte_start':start,
                'byte_end':end,'conversation_date':None,'role':role,'source_id':'src','turn_index':index}}
        proof['response']['fragment_projections']=[fragment('f0',0,first,'user',0),
            fragment('f1',first,second,'assistant',1),fragment('f2',second,len(content.encode()),'user',2)]
        self.assertTrue(v2_projection_proven(proof,session))
        changed=copy.deepcopy(proof);changed['response']['fragment_projections'][2]['fragment']['turn_index']=1
        self.assertFalse(v2_projection_proven(changed,session))
        single={'turns':[{'role':'user','content':'first'}]}
        proof['request']['source'].update(content='user: first',fragment_ids=['f0'],conversation_date=None)
        proof['response']['fragment_projections']=[{'id':'f0','content':'user: first','fragment':{
            'byte_start':0,'byte_end':11,'conversation_date':None,'role':None,'source_id':'src','turn_index':0}}]
        self.assertTrue(v2_projection_proven(proof,single))

    def test_rust_marker_variants_and_userless_fallback(self):
        session={'turns':[{'role':'assistant','content':'intro\n[ Human ] text\n**AI:** reply\nSYSTEM — note'},
                          {'role':'assistant','content':'end'}]}
        content='assistant: intro\n[ Human ] text\n**AI:** reply\nSYSTEM — note\nassistant: end'
        ranges=_known_turn_ranges(session,content)
        self.assertEqual([r[2] for r in ranges],['assistant','user','assistant','system','assistant'])
        self.assertEqual(ranges[0][0],0)
        self.assertEqual(ranges[-1][1],len(content.encode()))
        session={'turns':[{'role':'assistant','content':'one'},{'role':'assistant','content':'two'}]}
        self.assertEqual(_known_turn_ranges(session,'assistant: one\nassistant: two'),
                         [(0,len('assistant: one\nassistant: two'.encode()),None)])

    def test_rust_trim_differs_from_python_for_ascii_control_separators(self):
        for code in range(0x1c, 0x20):
            marker = '[' + chr(code) + 'user' + chr(code) + '] anything'
            self.assertIsNone(_marker_role(marker), hex(code))
        for marker in ('[\u0085user\u0085] x', '[ USER ] x', '** human **: x'):
            self.assertEqual(_marker_role(marker), 'user')
        session={'turns':[{'role':'assistant','content':'intro\n[\x1cuser\x1c] not a marker'},
                          {'role':'user','content':'end'}]}
        content='assistant: intro\n[\x1cuser\x1c] not a marker\nuser: end'
        ranges=_known_turn_ranges(session,content)
        self.assertEqual([r[2] for r in ranges],['assistant','user'])

    def test_utf8_offsets_are_bytes_and_must_be_character_boundaries(self):
        content='user: żółć\nuser: test'
        boundary=len('user: żółć\n'.encode())
        session={'turns':[{'role':'user','content':'żółć'},{'role':'user','content':'test'}]}
        proof={'request':{'source':{'source_id':'src','content':content,'fragment_ids':['f0'],'conversation_date':None}},
               'response':{'schema_version':2,'evidence':'fresh_immutable_ingest_projections_v2',
                 'fragment_projections':[{'id':'f0','content':'user: żółć\n','fragment':{'byte_start':0,
                   'byte_end':boundary,'conversation_date':None,'role':'user','source_id':'src','turn_index':0}}]}}
        self.assertTrue(v2_projection_proven(proof,session))
        proof['response']['fragment_projections'][0]['fragment']['byte_end']=7
        self.assertFalse(v2_projection_proven(proof,session))

    def test_public_alias_and_unrendered_session_are_handled(self):
        self.assertIs(known_turn_ranges, _known_turn_ranges)
        self.assertIsNone(known_turn_ranges({'turns': [{'role': 'user', 'content': 'a'}]}, 'user: b'))
        self.assertIsNone(known_turn_ranges({'turns': []}, ''))
        self.assertFalse(v2_projection_proven({}, {'turns': []}))


if __name__ == '__main__':
    unittest.main()
