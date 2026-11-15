// The phase 2 adapters (Browse, Analyze, snapshots, databases) on replies the real server
// gave (demo-redis and a Redis cluster, lists cut to two rows), and the routes they call.
import test from 'node:test';
import assert from 'node:assert/strict';
import {
  ROUTES, query, adaptName, adaptBrowse, adaptKeyList, adaptValue, adaptItem, adaptAnalysis, adaptProgress, adaptAnalysisInfo, adaptAnalysisStart,
  adaptBigKeys, adaptExpiry, adaptDiff, adaptHotKeys, adaptNodes, adaptSlots, adaptSnapshots, adaptSaved, adaptDbs, adaptDbSwitch, adaptState, adaptOverview,
  FINDING_WHERE, METHODS,
} from '../api.js';

// Replies of the real server (demo-redis and a Redis cluster), cut to two rows a list.
const REAL = {"browse":{"names":{"complete":true,"dbsize":74,"done":true,"error":null,"first_pages":8,"missing":[],"names_read":74,"nodes_done":null,"pages":1,"reading":false,"refused":[],"skipped":[]},"node":null,"nodes":[],"partial":null,"previous":false,"rows":[{"avg_bytes":null,"change":null,"command":"redis-cli --scan --pattern \"cart:*\"","est_bytes":null,"est_keys":60,"examples":[],"kind":"pattern","literal":false,"marked":false,"names_seen":60,"no_ttl":null,"no_ttl_ratio":null,"pattern":{"copy":"cart:{id}","id":"636172743a7b69647d","name":"cart:{id}"},"reason":null,"sampled_bytes":null,"sampled_keys":60,"share":0.8108108108108109,"size_max":null,"size_p50":null,"size_p99":null,"ttl_hist":null,"ttl_verdict":null,"type":null,"verdict":null,"warning":false},{"avg_bytes":null,"change":null,"command":"redis-cli --scan --pattern \"lock:job:*\"","est_bytes":null,"est_keys":5,"examples":[],"kind":"pattern","literal":false,"marked":false,"names_seen":5,"no_ttl":null,"no_ttl_ratio":null,"pattern":{"copy":"lock:job:{id}","id":"6c6f636b3a6a6f623a7b69647d","name":"lock:job:{id}"},"reason":null,"sampled_bytes":null,"sampled_keys":5,"share":0.06756756756756757,"size_max":null,"size_p50":null,"size_p99":null,"ttl_hist":null,"ttl_verdict":null,"type":null,"verdict":null,"warning":false}],"sizes":false,"sort":"keys","sorts":["keys","name"],"source":"names","total":74,"ttl_labels":["none","<1m"],"why":null},"browseAnalysis":{"names":null,"node":null,"nodes":[],"partial":null,"previous":false,"rows":[{"avg_bytes":4736,"change":null,"command":"redis-cli --scan --pattern \"events:log\"","est_bytes":4736,"est_keys":1,"examples":[{"bytes":4736,"copy":"events:log","freq":6,"id":"6576656e74733a6c6f67","name":"events:log","node":null,"ttl":"never","ttl_secs":null,"type":"stream"}],"kind":"pattern","literal":null,"marked":false,"names_seen":null,"no_ttl":1,"no_ttl_ratio":1,"pattern":{"copy":"events:log","id":"6576656e74733a6c6f67","name":"events:log"},"reason":"All 1 keys never expire and the pattern has no earlier snapshot to compare with. This looks intentional.","sampled_bytes":4736,"sampled_keys":1,"share":0.47935222672064776,"size_max":4736,"size_p50":4736,"size_p99":4736,"ttl_hist":[1,0],"ttl_verdict":"persistent · stable","type":"stream","verdict":"persistent","warning":false},{"avg_bytes":70,"change":null,"command":"redis-cli --scan --pattern \"cart:*\"","est_bytes":4240,"est_keys":60,"examples":[{"bytes":72,"copy":"cart:10","freq":6,"id":"636172743a3130","name":"cart:10","node":null,"ttl":"59m 59s","ttl_secs":3599,"type":"string"},{"bytes":72,"copy":"cart:11","freq":6,"id":"636172743a3131","name":"cart:11","node":null,"ttl":"59m 59s","ttl_secs":3599,"type":"string"}],"kind":"pattern","literal":null,"marked":false,"names_seen":null,"no_ttl":0,"no_ttl_ratio":0,"pattern":{"copy":"cart:{id}","id":"636172743a7b69647d","name":"cart:{id}"},"reason":"All 60 keys expire, typically in under an hour.","sampled_bytes":4240,"sampled_keys":60,"share":0.4291497975708502,"size_max":72,"size_p50":72,"size_p99":72,"ttl_hist":[0,0],"ttl_verdict":"ok","type":"string","verdict":"ok","warning":false}],"sizes":true,"sort":"memory","sorts":["memory","keys"],"source":"analysis","total":9880,"ttl_labels":["none","<1m"],"why":null},"browseNode":{"names":null,"node":0,"nodes":[{"endpoint":"127.0.0.1:49152","index":0,"role":"primary","sampled_keys":37,"status":"scanned"},{"endpoint":"127.0.0.1:49153","index":1,"role":"primary","sampled_keys":44,"status":"scanned"}],"partial":null,"previous":false,"rows":[{"avg_bytes":62,"change":null,"command":"redis-cli --scan --pattern \"item:*\"","est_bytes":1760,"est_keys":28,"examples":[],"kind":"pattern","literal":null,"marked":false,"names_seen":null,"no_ttl":28,"no_ttl_ratio":1,"pattern":{"copy":"item:{id}","id":"6974656d3a7b69647d","name":"item:{id}"},"reason":"All 28 keys never expire and the pattern has no earlier snapshot to compare with. This looks intentional.","sampled_bytes":1760,"sampled_keys":28,"share":0.7534246575342466,"size_max":64,"size_p50":64,"size_p99":64,"ttl_hist":[28,0],"ttl_verdict":"persistent · stable","type":"string","verdict":"persistent","warning":false},{"avg_bytes":64,"change":null,"command":"redis-cli --scan --pattern \"*:name\"","est_bytes":64,"est_keys":1,"examples":[],"kind":"pattern","literal":null,"marked":false,"names_seen":null,"no_ttl":1,"no_ttl_ratio":1,"pattern":{"copy":"{user0}:name","id":"7b75736572307d3a6e616d65","name":"{user0}:name"},"reason":"All 1 keys never expire and the pattern has no earlier snapshot to compare with. This looks intentional.","sampled_bytes":64,"sampled_keys":1,"share":0.0273972602739726,"size_max":64,"size_p50":64,"size_p99":64,"ttl_hist":[1,0],"ttl_verdict":"persistent · stable","type":"string","verdict":"persistent","warning":false}],"sizes":true,"sort":"memory","sorts":["memory","keys"],"source":"analysis","total":2336,"ttl_labels":["none","<1m"],"why":null},"keys":{"cluster":null,"done":true,"error":null,"find":null,"freq":true,"glob":"cart:*","kind":"pattern","lfu":true,"list":1,"loaded":60,"new_from":0,"other_pattern":0,"page_keys":1000,"pages":1,"pattern":{"copy":"cart:{id}","id":"636172743a7b69647d","name":"cart:{id}"},"refused":[],"rows":[{"bytes":72,"copy":"cart:57","encoding":"embstr","freq":5,"id":"636172743a3537","length":5,"length_unit":"bytes","name":"cart:57","node":null,"ttl":"59m 59s","ttl_secs":3599,"type":"string"},{"bytes":64,"copy":"cart:9","encoding":"embstr","freq":5,"id":"636172743a39","length":5,"length_unit":"bytes","name":"cart:9","node":null,"ttl":"59m 59s","ttl_secs":3599,"type":"string"}],"sizes":true,"unreadable":0},"find":{"cluster":null,"done":true,"error":null,"find":{"glob":"*user*","text":"user"},"freq":true,"glob":"*user*","kind":"find","lfu":true,"list":2,"loaded":1,"new_from":0,"other_pattern":0,"page_keys":1000,"pages":1,"pattern":null,"refused":[],"rows":[{"bytes":104,"copy":"user:1:profile","encoding":"listpack","freq":5,"id":"757365723a313a70726f66696c65","length":2,"length_unit":"fields","name":"user:1:profile","node":null,"ttl":"never","ttl_secs":null,"type":"hash"}],"sizes":true,"unreadable":0},"hash":{"capped":false,"cluster":false,"command":"HSCAN","cut":null,"done":true,"error":null,"field_ttls":true,"field_ttls_off":null,"gone":false,"items":[{"at":null,"fields":null,"index":0,"name":"name","ttl":"never","value":"Ada"},{"at":null,"fields":null,"index":1,"name":"email","ttl":"never","value":"ada@example.com"}],"key":{"copy":"user:1:profile","id":"757365723a313a70726f66696c65","name":"user:1:profile"},"kind":"hash","length":2,"loaded":2,"loading":false,"max_items":10000,"message":null,"meta":{"bytes":104,"copy":"user:1:profile","encoding":"listpack","freq":5,"id":"757365723a313a70726f66696c65","length":2,"length_unit":"fields","name":"user:1:profile","node":null,"ttl":"never","ttl_secs":null,"type":"hash"},"module":null,"more":false,"notice":"All 2 items are loaded.","pages":1,"pattern":null,"refused":[],"type":"hash","value":null,"view":4},"stream":{"capped":false,"cluster":false,"command":"XREVRANGE","cut":null,"done":true,"error":null,"field_ttls":true,"field_ttls_off":null,"gone":false,"items":[{"at":null,"fields":[{"name":"kind","value":"logout"}],"index":0,"name":"1794745502183-1","ttl":null,"value":null},{"at":null,"fields":[{"name":"kind","value":"login"},{"name":"who","value":"ada"}],"index":1,"name":"1794745502183-0","ttl":null,"value":null}],"key":{"copy":"events:log","id":"6576656e74733a6c6f67","name":"events:log"},"kind":"stream","length":null,"loaded":2,"loading":false,"max_items":10000,"message":null,"meta":{"bytes":null,"copy":"events:log","encoding":null,"freq":null,"id":"6576656e74733a6c6f67","length":null,"length_unit":"bytes","name":"events:log","node":null,"ttl":"never","ttl_secs":null,"type":"stream"},"module":null,"more":false,"notice":"All 2 items are loaded.","pages":1,"pattern":null,"refused":[],"type":"stream","value":null,"view":8},"binary":{"capped":false,"cluster":false,"command":"GETRANGE","cut":null,"done":true,"error":null,"field_ttls":true,"field_ttls_off":null,"gone":false,"items":[],"key":{"copy":null,"id":"6576696c1b5b33316d6b6579","name":"evil\\x1b[31mkey"},"kind":"string","length":1,"loaded":1,"loading":false,"max_items":10000,"message":null,"meta":{"bytes":64,"copy":null,"encoding":"embstr","freq":5,"id":"6576696c1b5b33316d6b6579","length":1,"length_unit":"bytes","name":"evil\\x1b[31mkey","node":null,"ttl":"never","ttl_secs":null,"type":"string"},"module":null,"more":false,"notice":"The value is loaded.","pages":1,"pattern":null,"refused":[],"type":"string","value":{"bytes":1,"compression":null,"copy":"v","copy_refused":null,"detected":"text","format":"text","formats":["text","hex"],"header":"value · text · 1 bytes · auto-detected","lines":["v"],"note":null,"picked":null,"truncated":false},"view":9},"item":{"field":null,"index":0,"name":null,"shown":{"bytes":7,"compression":null,"copy":null,"copy_refused":"The value has control characters, so it is not copied as text: show it as hex to copy it.","detected":"hex","format":"text","formats":["text","hex"],"header":"value · 7 bytes · shown as text","lines":["\\x00\\x01\\xffbin\\x1b"],"note":null,"picked":"text","truncated":false}},"info":{"analysis":{"db":0,"depth":null,"error":null,"note":null,"previous":false,"report":null,"run":3,"sampled":null,"started_unix":null,"status":"none","target":null},"estimates":[{"about":"as fast as the server answers","commands":303,"depth":"quick","duration_secs":null,"freq":true,"keys":74,"per_sec":null,"sample_limit":10000,"shards":0,"sizes":true},{"about":"as fast as the server answers","commands":303,"depth":"deep","duration_secs":null,"freq":true,"keys":74,"per_sec":null,"sample_limit":100000,"shards":0,"sizes":true}],"running":false},"started":{"depth":"quick","estimate":{"about":"as fast as the server answers","commands":303,"depth":"quick","duration_secs":null,"freq":true,"keys":74,"per_sec":null,"sample_limit":10000,"shards":0,"sizes":true},"run":4,"sample_limit":10000},"big":{"big_key_bytes":67108864,"empty":null,"facts_read":true,"notes":[],"recommendations":[{"hint":"A lock should always carry a TTL.","severity":"note","text":"lock:job:{id}: 100% of its keys never expire."},{"hint":"Fine for data that must stay; caches should expire.","severity":"info","text":"57% of memory never expires: 5.5K of 9.6K."}],"report":{"complete":true,"previous":false,"sampled_keys":74,"sizes_available":true,"stopped":false,"taken_at_unix":1794745502},"rows":[{"big":false,"encoding":"stream","key":{"bytes":4736,"copy":"events:log","freq":6,"id":"6576656e74733a6c6f67","name":"events:log","node":null,"ttl":"never","ttl_secs":null,"type":"stream"},"length":2,"length_unit":"entries","node":null,"pattern":{"copy":"events:log","id":"6576656e74733a6c6f67","name":"events:log"}},{"big":false,"encoding":"listpack","key":{"bytes":104,"copy":"user:1:profile","freq":6,"id":"757365723a313a70726f66696c65","name":"user:1:profile","node":null,"ttl":"never","ttl_secs":null,"type":"hash"},"length":2,"length_unit":"fields","node":null,"pattern":{"copy":"user:{id}:profile","id":"757365723a7b69647d3a70726f66696c65","name":"user:{id}:profile"}}]},"expiry":{"empty":null,"freed":{"later":0,"never":5640,"never_share":0.5708502024291497,"total":9880,"within":[{"bytes":4240,"label":"1 h"},{"bytes":4240,"label":"1 d"}]},"freed_off":null,"report":{"complete":true,"previous":false,"sampled_keys":74,"sizes_available":true,"stopped":false,"taken_at_unix":1794745502},"rows":[{"command":"redis-cli --scan --pattern \"events:log\"","est_bytes":4736,"est_keys":1,"explain":["All 1 keys never expire and the pattern has no earlier snapshot to compare with.","This looks intentional."],"no_ttl":1,"no_ttl_ratio":1,"pattern":{"copy":"events:log","id":"6576656e74733a6c6f67","name":"events:log"},"sampled_keys":1,"status":"persistent · stable","ttl_hist":[1,0],"warning":false},{"command":"redis-cli --scan --pattern \"lock:job:*\"","est_bytes":280,"est_keys":5,"explain":["All 5 keys never expire and the pattern has no earlier snapshot to compare with.","This looks intentional."],"no_ttl":5,"no_ttl_ratio":1,"pattern":{"copy":"lock:job:{id}","id":"6c6f636b3a6a6f623a7b69647d","name":"lock:job:{id}"},"sampled_keys":5,"status":"persistent · stable","ttl_hist":[5,0],"warning":false}],"ttl_labels":["none","<1m"]},"changes":{"report":{"complete":true,"previous":false,"sampled_keys":74,"sizes_available":true,"stopped":false,"taken_at_unix":1794745502},"status":"none","text":"No earlier snapshot for local. Save one; the next analysis compares against it."},"hot":{"empty":null,"hot_big_bytes":10485760,"hot_freq":100,"report":{"complete":true,"previous":false,"sampled_keys":74,"sizes_available":true,"stopped":false,"taken_at_unix":1794745502},"rows":[{"hot_and_big":false,"key":{"bytes":72,"copy":"str:plain","freq":9,"id":"7374723a706c61696e","name":"str:plain","node":null,"ttl":"never","ttl_secs":null,"type":"string"},"node":null,"pattern":{"copy":"str:plain","id":"7374723a706c61696e","name":"str:plain"}},{"hot_and_big":false,"key":{"bytes":80,"copy":"queue:jobs","freq":7,"id":"71756575653a6a6f6273","name":"queue:jobs","node":null,"ttl":"never","ttl_secs":null,"type":"list"},"node":null,"pattern":{"copy":"queue:jobs","id":"71756575653a6a6f6273","name":"queue:jobs"}}]},"nodesOff":{"off":"Not a cluster: this analysis read one standalone server."},"nodes":{"empty":null,"report":{"complete":true,"previous":false,"sampled_keys":120,"sizes_available":true,"stopped":false,"taken_at_unix":1794745506},"rows":[{"complete":true,"endpoint":"127.0.0.1:49152","id":"db60446aa32515e6de67dd00619ee413492b6997","index":0,"keys":37,"memory":3036424,"note":null,"patterns":10,"primary_id":"db60446aa32515e6de67dd00619ee413492b6997","role":"primary","sampled_keys":37,"sizes_available":true,"slot_count":5461,"slots":[[0,5460]],"status":"scanned","version":"7.4.12"},{"complete":true,"endpoint":"127.0.0.1:49153","id":"3b3054e0483ab994b03efee9ba34141ff865afbe","index":1,"keys":44,"memory":2988240,"note":null,"patterns":12,"primary_id":"3b3054e0483ab994b03efee9ba34141ff865afbe","role":"primary","sampled_keys":44,"sizes_available":true,"slot_count":5462,"slots":[[5461,10922]],"status":"scanned","version":"7.4.12"}]},"slotsOff":{"off":"CLUSTER SLOT-STATS needs Redis 8.2 or Valkey 8.0; 127.0.0.1:49152 runs Redis 7.4.12"},"compare":{"after_sampled_keys":75,"after_taken_at_unix":1794745503,"after_utc":"2026-11-15 12:25:03 UTC","before_sampled_keys":74,"before_taken_at_unix":1794745502,"before_utc":"2026-11-15 12:25:02 UTC","empty":null,"note":null,"rows":[{"bytes_after":72,"bytes_before":null,"change":{"alarming":true,"cause":"new","cause_words":"new","delta_bytes":72,"delta_keys":1},"keys_after":1,"keys_before":null,"partial":false,"pattern":{"copy":"cart:new","id":"636172743a6e6577","name":"cart:new"},"trend":null},{"bytes_after":80,"bytes_before":80,"change":{"alarming":false,"cause":"unchanged","cause_words":"unchanged","delta_bytes":0,"delta_keys":0},"keys_after":1,"keys_before":1,"partial":false,"pattern":{"copy":"board:scores","id":"626f6172643a73636f726573","name":"board:scores"},"trend":null}],"trend_points":null,"used_memory_after":2018832,"used_memory_before":1944104},"snapshots":{"can_save":true,"profile":"local","save_off":null,"saved":true,"snapshots":[{"current":true,"file":"20261115T122503Z.json","taken_at_unix":1794745503,"taken_at_utc":"2026-11-15 12:25:03 UTC"},{"current":false,"file":"20261115T122502Z.json","taken_at_unix":1794745502,"taken_at_utc":"2026-11-15 12:25:02 UTC"}]},"saved":{"file":"20261115T122502Z.json","sampled_keys":74,"taken_at_unix":1794745502,"taken_at_utc":"2026-11-15 12:25:02 UTC","text":"Saved snapshot 20261115T122502Z.json"},"dbs":{"current":0,"note":"Picking a database is for this session. The saved profile is unchanged.","rows":[{"current":true,"db":0,"expires":60,"keys":74},{"current":false,"db":1,"expires":0,"keys":3}],"saved_as":"local","unread":null},"dbsCluster":{"off":"A cluster only has database 0."},"switched":{"db":1,"run":2,"switched":true,"text":"Switched to db 1 for this session. The saved profile is unchanged."},"clusterKeys":{"cluster":{"at":2,"missing":[],"narrowed":null,"nodes_done":3,"walk":[0,1]},"done":true,"error":null,"find":null,"freq":false,"glob":"item:*","kind":"pattern","lfu":false,"list":1,"loaded":90,"new_from":0,"other_pattern":0,"page_keys":1000,"pages":3,"pattern":{"copy":"item:{id}","id":"6974656d3a7b69647d","name":"item:{id}"},"refused":[],"rows":[{"bytes":64,"copy":"item:18","encoding":"embstr","freq":null,"id":"6974656d3a3138","length":1,"length_unit":"bytes","name":"item:18","node":0,"ttl":"never","ttl_secs":null,"type":"string"},{"bytes":56,"copy":"item:6","encoding":"embstr","freq":null,"id":"6974656d3a36","length":1,"length_unit":"bytes","name":"item:6","node":0,"ttl":"never","ttl_secs":null,"type":"string"}],"sizes":true,"unreadable":0},"progress":[{"depth":"quick","kind":"started","run":4,"sample_limit":10000},{"kind":"sampling","patterns":null,"run":4,"sampled":0,"stopping":false,"target":74}]};

test('a name: the escaped text to show, the hex id to ask with, a copy only when escaping changed nothing', () => {
  assert.deepEqual(adaptName({ copy: 'cart:{id}', id: '636172743a7b69647d', name: 'cart:{id}' }), { name: 'cart:{id}', id: '636172743a7b69647d', copy: 'cart:{id}' });
  const evil = adaptName(REAL.binary.key);
  assert.equal(evil.name, 'evil\\x1b[31mkey', 'the escape character arrives escaped, as text');
  assert.equal(evil.id, '6576696c1b5b33316d6b6579');
  assert.equal(evil.copy, null, 'never copied: a pasted copy would name another key');
  assert.equal(adaptName({ name: 'x', id: 'zz<script>' }).id, '', 'an id that is not hex is dropped, never sent');
  assert.equal(adaptName({ name: '', id: '' }).name, '', 'an empty name stays empty, not unknown');
});

test('routes: a key or pattern is asked for by its hex id, every part encoded, nothing unknown sent', () => {
  assert.equal(query({ a: 'x y', b: null, c: undefined, d: 0 }), '?a=x%20y&d=0');
  assert.equal(query({}), '');
  assert.deepEqual(ROUTES.keys('636172743a7b69647d'), ['GET', '/api/keys?pattern=636172743a7b69647d']);
  assert.deepEqual(ROUTES.keys('61', 2), ['GET', '/api/keys?pattern=61&node=2']);
  assert.deepEqual(ROUTES.find('user:*:cart'), ['GET', '/api/find?q=user%3A*%3Acart']);
  assert.deepEqual(ROUTES.find('a&b=c#d'), ['GET', '/api/find?q=a%26b%3Dc%23d']);
  assert.deepEqual(ROUTES.value('6576696c1b5b33316d6b6579'), ['GET', '/api/value?key=6576696c1b5b33316d6b6579']);
  assert.deepEqual(ROUTES.valueItem({ view: 3, index: 0, format: 'JSON' }), ['GET', '/api/value/item?view=3&index=0&format=JSON']);
  assert.deepEqual(ROUTES.slots({ order: 'cpu', refresh: true }), ['GET', '/api/analysis/slots?order=cpu&refresh=1']);
  assert.deepEqual(ROUTES.snapshotCompare('20261115T122502Z.json', 'current'), ['GET', '/api/snapshots/compare?a=20261115T122502Z.json&b=current']);
  assert.deepEqual(ROUTES.browse({ sort: 'memory' }), ['GET', '/api/browse?sort=memory']);
  for (const m of ['getDbs', 'switchDb', 'getBrowse', 'moreNames', 'getKeys', 'findKeys', 'moreKeys', 'openValue', 'moreValue', 'getItem', 'getAnalysis', 'startAnalysis', 'stopAnalysis',
    'getBigKeys', 'getExpiry', 'getChanges', 'getHotKeys', 'getNodes', 'getSlots', 'saveSnapshot', 'listSnapshots', 'compareSnapshots']) assert.ok(METHODS.includes(m), m);
});

test('the tree from key names: counts only, how far the names are read', () => {
  const t = adaptBrowse(REAL.browse);
  assert.equal(t.source, 'names');
  assert.equal(t.sizes, false);
  assert.deepEqual(t.sorts, ['keys', 'name']);
  assert.equal(t.names.complete, true);
  assert.equal(t.names.namesRead, 74);
  assert.equal(t.names.reading, false);
  assert.equal(t.rows.length, 2);
  const r = t.rows[0];
  assert.equal(r.kind, 'pattern');
  assert.equal(typeof r.pattern.id, 'string');
  assert.equal(r.estBytes, null, 'no sizes before an analysis: unknown, not zero');
  assert.equal(adaptBrowse({ source: 'weird' }).source, 'none');
  assert.deepEqual(adaptBrowse({ sorts: ['keys', 'evil'] }).sorts, ['keys']);
});

test('the tree from an analysis, and narrowed to a cluster node', () => {
  const t = adaptBrowse(REAL.browseAnalysis);
  assert.equal(t.source, 'analysis');
  assert.equal(t.sizes, true);
  assert.equal(typeof t.rows[0].estBytes, 'number');
  assert.ok(t.rows[0].ttlHist.every((n) => typeof n === 'number'), 'a TTL histogram of counts');
  assert.deepEqual(t.ttlLabels, ['none', '<1m']);
  const n = adaptBrowse(REAL.browseNode);
  assert.equal(n.node, 0);
  assert.equal(n.nodes[0].endpoint, '127.0.0.1:49152');
  assert.equal(n.nodes[0].status, 'scanned');
});

test('a key list: rows with their facts, where new rows start, a cluster\'s walk', () => {
  const l = adaptKeyList(REAL.keys);
  assert.equal(l.kind, 'pattern');
  assert.equal(l.list, 1);
  assert.equal(l.loaded, 60);
  assert.equal(l.newFrom, 0);
  assert.equal(l.pageKeys, 1000);
  assert.equal(l.capped, false, 'a reply from before capped was added reads as not full');
  const full = adaptKeyList({ ...REAL.keys, capped: true, max_rows: 50000 });
  assert.deepEqual([full.capped, full.maxRows], [true, 50000]);
  assert.equal(l.pattern.name, 'cart:{id}');
  assert.deepEqual([l.rows[0].type, l.rows[0].ttl, l.rows[0].ttlSecs, l.rows[0].encoding, l.rows[0].lengthUnit], ['string', '59m 59s', 3599, 'embstr', 'bytes']);
  const f = adaptKeyList(REAL.find);
  assert.equal(f.kind, 'find');
  assert.deepEqual(f.find, { text: 'user', glob: '*user*' });
  const c = adaptKeyList(REAL.clusterKeys);
  assert.equal(c.cluster.nodesDone, 3);
  assert.equal(c.rows[0].node, 0);
});

test('a value: its kind, items, facts and the renderings offered', () => {
  const v = adaptValue(REAL.hash);
  assert.equal(v.kind, 'hash');
  assert.equal(v.fieldTtls, true);
  assert.deepEqual(v.items.map((i) => [i.name, i.value]), [['name', 'Ada'], ['email', 'ada@example.com']]);
  assert.equal(v.meta.lengthUnit, 'fields');
  const s = adaptValue(REAL.stream);
  assert.equal(s.kind, 'stream');
  assert.deepEqual(s.items[1].fields, [{ name: 'kind', value: 'login' }, { name: 'who', value: 'ada' }]);
  const b = adaptValue(REAL.binary);
  assert.equal(b.kind, 'string');
  assert.equal(b.key.copy, null);
  assert.deepEqual(b.value.formats.slice(0, 2), ['text', 'hex']);
  assert.equal(b.value.lines[0], 'v');
  const i = adaptItem(REAL.item);
  assert.equal(i.shown.copy, null, 'control characters: not copied as text');
  assert.match(i.shown.copyRefused, /show it as hex/);
  assert.equal(adaptValue({ kind: 'something new' }).kind, 'other');
});

test('the analysis: state, estimates, the start reply, each progress event', () => {
  const info = adaptAnalysisInfo(REAL.info);
  assert.equal(info.analysis.status, 'none');
  assert.equal(info.analysis.running, false);
  assert.deepEqual(info.estimates.map((e) => [e.depth, e.sampleLimit, e.keys, e.commands, e.about]), [['quick', 10000, 74, 303, 'as fast as the server answers'], ['deep', 100000, 74, 303, 'as fast as the server answers']]);
  const started = adaptAnalysisStart(REAL.started);
  assert.equal(started.depth, 'quick');
  assert.equal(started.estimate.commands, 303);
  const a = adaptAnalysis({ status: 'sampling', run: 4, depth: 'quick', sampled: 30, target: 74, report: { taken_at_unix: 1794745502, sampled_keys: 74, complete: true }, previous: true });
  assert.equal(a.running, true);
  assert.equal(a.previous, true);
  assert.equal(a.report.sampledKeys, 74);
  assert.equal(adaptAnalysis({ status: 'exploded' }).status, 'none');
  assert.equal(adaptAnalysis({ status: 'stopping' }).running, true);
  const events = [
    { depth: 'quick', kind: 'started', run: 4, sample_limit: 10000 },
    { kind: 'sampling', patterns: null, run: 4, sampled: 0, stopping: false, target: 74 },
    { complete: true, kind: 'done', patterns: 11, run: 4, sampled_keys: 74, stopped: false, taken_at_unix: 1794745502 },
    { kind: 'compared', run: 4 },
    { kind: 'stopping', run: 6 },
    { kept: false, kind: 'stopped', run: 6, text: 'Analysis stopped before it sampled any key. Nothing changed.' },
    { run: 7, kind: 'failed', error: 'the connection to the server was lost' },
  ].map(adaptProgress);
  assert.deepEqual(events.map((e) => e.kind), ['started', 'sampling', 'done', 'compared', 'stopping', 'stopped', 'failed']);
  assert.equal(events[1].target, 74);
  assert.equal(events[2].sampledKeys, 74);
  assert.equal(events[5].kept, false);
  assert.equal(events[6].error, 'the connection to the server was lost');
  assert.equal(adaptState({ status: 'connected', analysis: { status: 'done', run: 2, db: 1 } }).analysis.db, 1);
  assert.equal(adaptState({}).analysis.status, 'none', 'an older server: no analysis yet');
});

test('Big keys and Hot keys: the row\'s node is an endpoint, the key\'s node an index', () => {
  const big = adaptBigKeys(REAL.big);
  assert.equal(big.rows[0].key.name, 'events:log');
  assert.equal(big.rows[0].lengthUnit, 'entries');
  assert.equal(big.rows[0].endpoint, null);
  assert.equal(big.recommendations[0].hint, 'A lock should always carry a TTL.');
  assert.equal(big.bigKeyBytes, 67108864);
  const onCluster = adaptBigKeys({ rows: [{ key: { name: 'k', id: '6b', node: 2 }, node: '127.0.0.1:7003', pattern: { name: 'k', id: '6b' } }] });
  assert.equal(onCluster.rows[0].endpoint, '127.0.0.1:7003');
  assert.equal(onCluster.rows[0].key.node, 2);
  const hot = adaptHotKeys(REAL.hot);
  assert.equal(hot.rows[0].key.freq, 9);
  assert.equal(hot.hotFreq, 100);
  assert.equal(adaptHotKeys({ off: 'The maxmemory policy is noeviction: no LFU counters.' }).off, 'The maxmemory policy is noeviction: no LFU counters.');
});

test('Expiry: patterns by TTL risk and the memory TTLs free', () => {
  const e = adaptExpiry(REAL.expiry);
  assert.equal(e.freed.never, 5640);
  assert.equal(e.freed.total, 9880);
  assert.deepEqual(e.freed.within.map((w) => w.label), ['1 h', '1 d']);
  assert.equal(e.rows[0].status, 'persistent · stable');
  assert.equal(e.rows[0].command, 'redis-cli --scan --pattern "events:log"');
  assert.equal(adaptExpiry({ freed_off: 'no sizes' }).freed, null);
});

test('Changes and a comparison: no earlier snapshot, or each pattern before and after', () => {
  const none = adaptDiff(REAL.changes);
  assert.equal(none.status, 'none');
  assert.match(none.text, /^No earlier snapshot for local/);
  const d = adaptDiff(REAL.compare);
  assert.equal(d.status, 'diff', 'a comparison has rows and no status: it is a diff');
  assert.equal(d.before.utc, '2026-11-15 12:25:02 UTC');
  assert.equal(d.after.sampledKeys, 75);
  assert.equal(d.after.usedMemory, 2018832);
  assert.deepEqual([d.rows[0].pattern.name, d.rows[0].change.cause, d.rows[0].change.alarming, d.rows[0].bytesBefore], ['cart:new', 'new', true, null]);
  const t = adaptDiff({ status: 'diff', rows: [{ pattern: { name: 'a', id: '61' }, trend: { values: [1, null, 3], growth: { per_day: 2, ratio_per_day: 0.5 } } }] });
  assert.deepEqual(t.rows[0].trend, { values: [1, null, 3], perDay: 2, ratioPerDay: 0.5 });
});

test('Nodes and Slots: off on a standalone server or an old cluster, in the server\'s words', () => {
  assert.equal(adaptNodes(REAL.nodesOff).off, 'Not a cluster: this analysis read one standalone server.');
  const n = adaptNodes(REAL.nodes);
  assert.deepEqual([n.rows[0].endpoint, n.rows[0].status, n.rows[0].slotCount, n.rows[0].role], ['127.0.0.1:49152', 'scanned', 5461, 'primary']);
  assert.equal(typeof n.rows[0].slots, 'string');
  assert.match(adaptSlots(REAL.slotsOff).off, /^CLUSTER SLOT-STATS needs Redis 8.2 or Valkey 8.0/);
  const s = adaptSlots({ order: 'cpu', hottest: [{ slot: 61, node: 0, endpoint: 'a:1', keys: 3, cpu_usec: 9, sampled_keys: [{ key: { name: 'k', id: '6b' }, pattern: { name: 'k', id: '6b' }, kinds: 'hash' }] }], ranges: [], memory_column: false });
  assert.equal(s.order, 'cpu');
  assert.equal(s.hottest[0].sampled[0].key.name, 'k');
  assert.equal(s.hottest[0].netIn, null, 'a figure not given is unknown');
});

test('snapshots, a save, the databases and a switch', () => {
  const l = adaptSnapshots(REAL.snapshots);
  assert.equal(l.canSave, true);
  assert.equal(l.profile, 'local');
  assert.deepEqual(l.snapshots.map((s) => [s.file, s.current]), [['20261115T122503Z.json', true], ['20261115T122502Z.json', false]]);
  assert.deepEqual(adaptSnapshots({ snapshots: [{ file: '' }, {}] }).snapshots, [], 'a snapshot without a file name is dropped');
  const s = adaptSaved(REAL.saved);
  assert.equal(s.text, 'Saved snapshot 20261115T122502Z.json');
  const d = adaptDbs(REAL.dbs);
  assert.deepEqual(d.rows.map((r) => [r.db, r.keys, r.current]), [[0, 74, true], [1, 3, false]]);
  assert.equal(adaptDbs(REAL.dbsCluster).off, 'A cluster only has database 0.');
  const sw = adaptDbSwitch(REAL.switched);
  assert.deepEqual([sw.switched, sw.db, sw.run], [true, 1, 2]);
  assert.equal(adaptDbSwitch({ switched: false, db: 0 }).switched, false);
});

test('Overview\'s analysis part, and where each finding is looked into', () => {
  const o = adaptOverview({ analysis: { taken_at_unix: 1794745502, sampled_keys: 74, complete: true, sizes_available: true, changes: 'none', never_expires_share: 0.57,
    findings: [{ area: 'Analyze', panel: 'Expiry', screen: 'expiry', severity: 'note', text: 'lock:job:{id}: 100% of its keys never expire.' }],
    top_patterns: [{ est_bytes: 4736, est_keys: 1, pattern: { copy: 'events:log', id: '6576656e74733a6c6f67', name: 'events:log' }, share: 0.479 }] } });
  assert.equal(o.analysis.findings[0].where, 'analyze/expiry');
  assert.equal(o.analysis.topPatterns[0].pattern.name, 'events:log');
  assert.equal(o.analysis.changes, 'none');
  assert.equal(adaptOverview({}).analysis, null);
  for (const where of Object.values(FINDING_WHERE)) assert.match(where, /^(overview|browse|analyze|server|pubsub|connections)(\/[a-z_]+)?$/, 'only areas this page has');
});
