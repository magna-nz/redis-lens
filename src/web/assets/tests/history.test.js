// The command line: history, the masked echo, and what a reply shows in each view.
import test from 'node:test';
import assert from 'node:assert/strict';
import { CommandHistory, maskCommand } from '../ui.js';
import { replyBody, capEntries, completeLine, helpTarget, helpLines } from '../views/cmdline.js';

test('up and down step through the commands and come back to the line being typed', () => {
  const h = new CommandHistory();
  h.add('PING');
  h.add('DBSIZE');
  h.add('TTL user:1');
  assert.equal(h.prev('GE'), 'TTL user:1');
  assert.equal(h.prev('TTL user:1'), 'DBSIZE');
  assert.equal(h.prev('DBSIZE'), 'PING');
  assert.equal(h.prev('PING'), 'PING', 'stops at the oldest');
  assert.equal(h.next('PING'), 'DBSIZE');
  assert.equal(h.next('DBSIZE'), 'TTL user:1');
  assert.equal(h.next('TTL user:1'), 'GE', 'the draft comes back');
  assert.equal(h.next('GE'), 'GE', 'nothing newer');
});

test('an empty history leaves the line alone', () => {
  const h = new CommandHistory();
  assert.equal(h.prev('abc'), 'abc');
  assert.equal(h.next('abc'), 'abc');
});

test('blank lines and immediate repeats are not kept, and the cap holds', () => {
  const h = new CommandHistory(3);
  h.add('  ');
  h.add('PING');
  h.add('PING');
  assert.deepEqual(h.items, ['PING']);
  h.add('A');
  h.add('B');
  h.add('C');
  assert.deepEqual(h.items, ['A', 'B', 'C']);
});

test('adding a command starts stepping from the newest again', () => {
  const h = new CommandHistory();
  h.add('ONE');
  h.add('TWO');
  h.prev('');
  h.prev('');
  h.add('THREE');
  assert.equal(h.prev(''), 'THREE');
});

test('credentials in a typed command are masked', () => {
  assert.equal(maskCommand('AUTH hunter2'), 'AUTH ***');
  assert.equal(maskCommand('auth alice hunter2'), 'auth *** ***');
  assert.equal(maskCommand('HELLO 3 AUTH alice hunter2'), 'HELLO 3 AUTH *** ***');
  assert.equal(maskCommand('ACL SETUSER bob on >s3cret ~* +get'), 'ACL SETUSER bob on >*** ~* +get');
  assert.equal(maskCommand('ACL SETUSER bob #abcdef <old !x'), 'ACL SETUSER bob #*** <*** !***');
  assert.equal(maskCommand('CONFIG SET requirepass s3cret'), 'CONFIG SET requirepass ***');
  assert.equal(maskCommand('config set maxmemory 1gb masterauth s3cret'), 'config set maxmemory 1gb masterauth ***');
  assert.equal(maskCommand('MIGRATE h 6379 k 0 5000 AUTH s3cret'), 'MIGRATE h 6379 k 0 5000 AUTH ***');
  assert.equal(maskCommand('MIGRATE h 6379 k 0 5000 AUTH2 alice s3cret'), 'MIGRATE h 6379 k 0 5000 AUTH2 *** ***');
  assert.equal(maskCommand('SENTINEL SET mymaster auth-pass s3cret'), 'SENTINEL SET mymaster auth-pass ***');
  assert.equal(maskCommand('3 AUTH hunter2'), '3 AUTH ***', 'after a repeat count too');
});

test('ordinary commands are left as typed', () => {
  for (const line of ['GET user:1', 'CONFIG GET maxmemory', 'SET auth hunter2', 'HGETALL  user:10293:cart', '']) {
    assert.equal(maskCommand(line), line);
  }
});

test('a reply shows the view asked for, or its text with a note', () => {
  const views = { text: '1) "a"\n2) "1"', table: '#  field  value\n1  a      1', json: '{\n  "a": "1"\n}', raw: null };
  assert.deepEqual(replyBody(views, 'text'), { text: views.text, note: null });
  assert.deepEqual(replyBody(views, 'table'), { text: views.table, note: null });
  assert.equal(replyBody(views, 'json').text, views.json);
  const raw = replyBody(views, 'raw');
  assert.equal(raw.text, views.text);
  assert.match(raw.note, /no raw view/);
  assert.match(replyBody({ text: 'PONG', table: null, json: null, raw: null }, 'json').note, /no JSON view/);
});

const NAMES = ['acl', 'acl|cat', 'config', 'config|get', 'config|help', 'config|set', 'get', 'getdel', 'getex', 'hget', 'hgetall', 'ping'];

test('tab completes command names, in the case that was typed', () => {
  assert.deepEqual(completeLine('ge', NAMES), ['get', 'getdel', 'getex']);
  assert.deepEqual(completeLine('GE', NAMES), ['GET', 'GETDEL', 'GETEX']);
  assert.deepEqual(completeLine('Hge', NAMES), ['HGET', 'HGETALL']);
  assert.deepEqual(completeLine('p', NAMES), ['ping']);
  assert.deepEqual(completeLine('con', NAMES), ['config'], 'subcommands are not offered as command names');
  assert.deepEqual(completeLine('zz', NAMES), []);
  assert.deepEqual(completeLine('', NAMES), [], 'nothing typed, nothing to complete');
});

test('tab completes the subcommands of a container command', () => {
  assert.deepEqual(completeLine('CONFIG ', NAMES), ['CONFIG GET', 'CONFIG HELP', 'CONFIG SET']);
  assert.deepEqual(completeLine('config s', NAMES), ['config set']);
  assert.deepEqual(completeLine('CONFIG g', NAMES), ['CONFIG get'], 'the second word keeps its own case');
  assert.deepEqual(completeLine('GET ', NAMES), [], 'a plain command has none');
  assert.deepEqual(completeLine('CONFIG GET max', NAMES), [], 'arguments are not completed');
});

test('tab keeps a repeat count in front, and needs the command list', () => {
  assert.deepEqual(completeLine('3 ge', NAMES), ['3 get', '3 getdel', '3 getex']);
  assert.deepEqual(completeLine('3 config s', NAMES), ['3 config set']);
  assert.deepEqual(completeLine('ge', []), []);
  assert.deepEqual(completeLine('ge', null), []);
});

test('the helper looks up the command once its name is typed out', () => {
  assert.equal(helpTarget('GET'), null, 'still being typed');
  assert.equal(helpTarget('GET '), 'get');
  assert.equal(helpTarget('  get user:1'), 'get');
  assert.equal(helpTarget('3 GET user:1'), 'get', 'a count in front is not the command');
  assert.equal(helpTarget(''), null);
  assert.equal(helpTarget('3 '), null);
});

test('the helper shows the arguments and complexity, then the summary; a subcommand shows its own', () => {
  const get = { name: 'get', summary: 'Returns the string value of a key.', complexity: 'O(1)', arguments: 'key', subcommands: [] };
  assert.deepEqual(helpLines(get, 'GET '), { title: 'GET key · O(1)', summary: 'Returns the string value of a key.' });
  const config = {
    name: 'config', summary: 'A container for server configuration commands.', complexity: 'Depends on subcommand.', arguments: null,
    subcommands: [{ name: 'config|get', summary: 'Returns the effective values of configuration parameters.', complexity: 'O(N)', arguments: 'parameter [parameter ...]', subcommands: [] }],
  };
  assert.deepEqual(helpLines(config, 'CONFIG '), { title: 'CONFIG · Depends on subcommand.', summary: 'A container for server configuration commands.' });
  assert.deepEqual(helpLines(config, 'config get maxmemory'), { title: 'CONFIG GET parameter [parameter ...] · O(N)', summary: 'Returns the effective values of configuration parameters.' });
  assert.deepEqual(helpLines(config, '2 CONFIG GET x').title, 'CONFIG GET parameter [parameter ...] · O(N)');
  assert.equal(helpLines(null, 'NOPE '), null);
});

test('the output keeps the last 100 commands', () => {
  const entries = Array.from({ length: 130 }, (_, i) => ({ input: `C${i}` }));
  const kept = capEntries(entries);
  assert.equal(kept.length, 100);
  assert.equal(kept[0].input, 'C30');
  assert.equal(capEntries(entries.slice(0, 5)).length, 5);
});
