// The add-connection form's pure helpers: taking a pasted password out of the address,
// splitting a token command into arguments, tags, the obvious-mistake checks, the router.
import test from 'node:test';
import assert from 'node:assert/strict';
import { splitArgs, parseTags, splitUrlSecret, urlHasSecret, checkAddForm, parseRoute, addressProblem, ADDRESS_TEXT } from '../ui.js';
import { guessName } from '../views/connections.js';

test('a password pasted in a URL moves out of the address', () => {
  assert.deepEqual(splitUrlSecret('rediss://app:s3cret@cache-02.prod.internal:6380/0'), { url: 'rediss://app@cache-02.prod.internal:6380/0', password: 's3cret' });
  assert.deepEqual(splitUrlSecret('redis://:s3cret@host:6379'), { url: 'redis://host:6379', password: 's3cret' });
  assert.deepEqual(splitUrlSecret('REDIS://u:p@h'), { url: 'REDIS://u@h', password: 'p' });
  assert.deepEqual(splitUrlSecret('redis://u:a@b@host/1'), { url: 'redis://u@host/1', password: 'a@b' }, 'the user info ends at the last @, as the server reads it');
});

test('an address with no password is left alone', () => {
  for (const s of ['redis://host:6379', 'redis://user@host', 'rediss://[::1]:6380/2', 'redis://u:@h', 'host:6379,ssl=True', '']) {
    assert.deepEqual(splitUrlSecret(s), { url: s, password: null }, s);
  }
});

test('anything that is not clear-cut is left for the server to read', () => {
  const escaped = 'redis://u:p%40ss@host';
  assert.deepEqual(splitUrlSecret(escaped), { url: escaped, password: null }, 'percent-escapes are decoded by the server, not here');
  const query = 'redis://host?password=s3cret';
  assert.deepEqual(splitUrlSecret(query), { url: query, password: null });
  const other = 'https://u:p@example.com';
  assert.deepEqual(splitUrlSecret(other), { url: other, password: null });
});

test('a StackExchange.Redis password option moves out too', () => {
  assert.deepEqual(splitUrlSecret('h:6380,password=s3cret,ssl=True'), { url: 'h:6380,ssl=True', password: 's3cret' });
  assert.deepEqual(splitUrlSecret('h:6380, Password = abc= ,user=app'), { url: 'h:6380,user=app', password: 'abc=' });
  assert.deepEqual(splitUrlSecret('h:6380,password='), { url: 'h:6380,password=', password: null });
});

test('an address that still holds a password is known, so the form can empty it after sending', () => {
  for (const s of ['redis://u:p%40ss@host', 'redis://host?password=s3cret', 'redis://:p@h', 'h:6380,password=x', 'h:6380, Password = x']) {
    assert.equal(urlHasSecret(s), true, s);
  }
  for (const s of ['redis://host:6379', 'redis://user@host', 'rediss://[::1]:6380/2', 'redis://u:@h', 'h:6380,ssl=True', 'h:6380,password=', '']) {
    assert.equal(urlHasSecret(s), false, s);
  }
  for (const s of ['rediss://app:s3cret@h:6380/0', 'h:6380,password=s3cret,ssl=True']) {
    assert.equal(urlHasSecret(splitUrlSecret(s).url), false, 'nothing is left once the password moved out');
  }
});

test('valkey:// and valkeys:// are read like redis:// and rediss://: the password moves out', () => {
  assert.deepEqual(splitUrlSecret('valkey://app:s3cret@cache:6379/0'), { url: 'valkey://app@cache:6379/0', password: 's3cret' });
  assert.deepEqual(splitUrlSecret('valkeys://:s3cret@cache:6380'), { url: 'valkeys://cache:6380', password: 's3cret' });
  assert.deepEqual(splitUrlSecret('VALKEYS://u:a@b@host/1'), { url: 'VALKEYS://u@host/1', password: 'a@b' });
  assert.equal(urlHasSecret('valkey://u:p@h'), true);
  assert.equal(urlHasSecret('valkeys://u@h'), false);
  assert.equal(addressProblem('valkeys://u:p@h:6380/0'), null);
  assert.equal(guessName('valkey://cache-07.internal:6379'), 'cache-07');
});

test('an address redis-lens will not read is refused before sending, in its words, and never split', () => {
  for (const bad of ['http://u:pw@host:6379', 'https://host', 'ftp://h', 'redis+sentinel://h:26379', 'unix:///tmp/redis.sock', 'host:6379,ssl=true,x=http://u:p@evil']) {
    assert.equal(addressProblem(bad), ADDRESS_TEXT.scheme, bad);
    assert.deepEqual(splitUrlSecret(bad), { url: bad, password: null }, `${bad} is not split`);
    assert.equal(checkAddForm({ url: bad, name: 'n' }).url, ADDRESS_TEXT.scheme, bad);
  }
  for (const bad of ['u:pw@host:6379', 'user@host', 'h1:6379,u:p@h2:6379', 'redis://host:6379/0?x=a@b', 'redis://host/u:p@x']) {
    assert.equal(addressProblem(bad), ADDRESS_TEXT.userinfo, bad);
    assert.equal(splitUrlSecret(bad).password, null, `${bad} is not split`);
  }
  assert.match(ADDRESS_TEXT.scheme, /only redis, rediss, valkey and valkeys URLs are supported/);
  for (const good of ['redis://u:p@h:1/0', 'rediss://h', 'valkey://h', 'host:6379', 'host:6379,password=p@ss,ssl=True', '[::1]:6379', '']) {
    assert.equal(addressProblem(good), null, good);
  }
  assert.deepEqual(splitUrlSecret('host:6379,password=p@ss'), { url: 'host:6379', password: 'p@ss' }, 'an @ in the password option is the password\'s');
});

test('a token command is split into arguments like a shell', () => {
  assert.deepEqual(splitArgs('aws elasticache generate-iam-auth-token --cache "my cache"').args, ['aws', 'elasticache', 'generate-iam-auth-token', '--cache', 'my cache']);
  assert.deepEqual(splitArgs("  sh -c 'echo  $TOKEN'  ").args, ['sh', '-c', 'echo  $TOKEN']);
  assert.deepEqual(splitArgs('a\\ b "c\\"d" \'\'').args, ['a b', 'c"d', '']);
  assert.deepEqual(splitArgs('').args, []);
  assert.equal(splitArgs('echo "open').error, 'A quote is left open.');
});

test('tags: commas or spaces, trimmed, no blanks, no repeats', () => {
  assert.deepEqual(parseTags(' cache, eu ,cache  team-a,,'), ['cache', 'eu', 'team-a']);
  assert.deepEqual(parseTags(''), []);
  assert.deepEqual(parseTags(null), []);
});

test('the add form: a good one has no errors', () => {
  assert.deepEqual(checkAddForm({ url: 'rediss://h:6380', name: 'cache-02', hasPassword: true }), {});
  assert.deepEqual(checkAddForm({
    url: 'redis://h', name: 'x', tokenCommand: 'aws token', hasPassword: false, sshHost: 'ops@bastion:2222', sshIdentity: '~/.ssh/id_ed25519',
    sentinelAddresses: 's1:26379, s2:26379', sentinelMaster: 'mymaster', hasSentinelPassword: true,
  }), {});
});

test('the add form: obvious mistakes, by field', () => {
  const empty = checkAddForm({ url: '', name: '' });
  assert.ok(empty.url);
  assert.ok(empty.name);
  assert.ok(checkAddForm({ url: 'redis://h extra', name: 'x' }).url);
  assert.match(checkAddForm({ url: 'redis://h', name: 'Docker:mine' }).name, /docker:/);
  assert.ok(checkAddForm({ url: 'redis://h', name: 'has space' }).name);
  assert.match(checkAddForm({ url: 'redis://h', name: 'x', tokenCommand: 'aws token', hasPassword: true }).tokenCommand, /one or the other/);
  assert.match(checkAddForm({ url: 'redis://h', name: 'x', tokenCommand: 'echo "open' }).tokenCommand, /quote/);
  assert.ok(checkAddForm({ url: 'redis://h', name: 'x', sshHost: '-oProxyCommand=x' }).sshHost);
  assert.ok(checkAddForm({ url: 'redis://h', name: 'x', sshIdentity: '~/.ssh/id' }).sshHost, 'an identity file needs a bastion');
  assert.ok(checkAddForm({ url: 'redis://h', name: 'x', sshHost: 'b', sshIdentity: '/k/%h' }).sshIdentity);
  assert.ok(checkAddForm({ url: 'redis://h', name: 'x', sentinelAddresses: 's1:26379' }).sentinelMaster);
  assert.ok(checkAddForm({ url: 'redis://h', name: 'x', sentinelMaster: 'mymaster' }).sentinelAddresses);
  assert.ok(checkAddForm({ url: 'redis://h', name: 'x', sentinelAddresses: 's1', sentinelMaster: 'm' }).sentinelAddresses);
  assert.ok(checkAddForm({ url: 'redis://h', name: 'x', hasSentinelPassword: true }).sentinelAddresses);
  assert.ok(checkAddForm({ url: 'redis://h', name: 'x', sentinelUser: 'sen' }).sentinelAddresses, 'a Sentinel username needs the Sentinels');
  assert.ok(checkAddForm({ url: 'redis://h', name: 'x', sentinelTls: true }).sentinelAddresses);
  assert.match(checkAddForm({ url: 'redis://h', name: 'x', entra: true, hasPassword: true }).entra, /Entra ID replaces/);
  assert.match(checkAddForm({ url: 'redis://h', name: 'x', entra: true, tokenCommand: 'get-token' }).entra, /Entra ID replaces/);
  assert.deepEqual(checkAddForm({ url: 'rediss://x.redis.azure.net:10000', name: 'az', entra: true }), {});
});

test('the checks never see a password, only whether there is one', () => {
  const errors = checkAddForm({ url: 'redis://h', name: 'x', tokenCommand: 'cmd', hasPassword: true, password: 'hunter2' });
  assert.equal(JSON.stringify(errors).includes('hunter2'), false);
});

test('a name is guessed from the address and never starts with docker:', () => {
  assert.equal(guessName('rediss://app@cache-02.prod.internal:6380/0'), 'cache-02');
  assert.equal(guessName('redis://127.0.0.1:6390'), 'local-6390');
  assert.equal(guessName('redis://localhost'), 'local-6379');
  assert.equal(guessName('mycache.redis.cache.windows.net:6380,ssl=True'), 'mycache');
  assert.equal(guessName('redis://[::1]:7000'), 'local-7000');
  assert.equal(guessName(''), '');
  assert.equal(/^docker:/i.test(guessName('redis://docker:6379')), false);
});

test('the router reads the hash', () => {
  const names = ['overview', 'server', 'pubsub', 'connections'];
  assert.deepEqual(parseRoute('#/server/slowlog', names, 'connections'), { name: 'server', sub: 'slowlog' });
  assert.deepEqual(parseRoute('#/overview', names, 'connections'), { name: 'overview', sub: null });
  assert.deepEqual(parseRoute('', names, 'connections'), { name: 'connections', sub: null });
  assert.deepEqual(parseRoute('#/nope/x', names, 'overview'), { name: 'overview', sub: null });
  assert.deepEqual(parseRoute('#server', names, 'overview'), { name: 'server', sub: null });
});
