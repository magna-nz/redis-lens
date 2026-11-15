// Connections: the home screen. Saved connections and the Redis or Valkey containers
// found in Docker, as cards; connect, test, edit, remove, and the add dialog.
//
// Rule 1: no credential is ever drawn here, because none is ever sent to the page. The
// password boxes in the add dialog are read once, at submit, straight into the request
// body, and emptied as soon as the request returns (success or not). They are never
// logged, never put in the address or in localStorage, and never echoed back.

import {
  h, fill, refill, badge, envBadge, emptyState, notice, openDialog, confirmDialog, errorText,
  parseTags, splitArgs, splitUrlSecret, urlHasSecret, urlWantsTls, checkAddForm,
} from '../ui.js';

const STATUS = {
  'not-open': ['○', 'not open'],
  connecting: ['●', 'connecting…'],
  connected: ['●', 'connected'],
  limited: ['▲', 'limited'],
  failed: ['✗', 'failed'],
};

const ENVS = ['PROD', 'STAGE', 'LOCAL', 'OTHER'];
const PROVIDERS = [
  ['', 'Guess from the host name'],
  ['elasticache', 'ElastiCache'],
  ['elasticache-serverless', 'ElastiCache Serverless'],
  ['memorydb', 'MemoryDB'],
  ['azure', 'Azure'],
  ['memorystore', 'Memorystore'],
];

const WRITES_HINT = 'With writes on, key edits, deletes and write commands reach the server. Deletes, bulk actions and @dangerous commands still ask each time.';

let fieldSeq = 0;
const nextId = (name) => `f-${name}-${(fieldSeq += 1)}`;

export function mount(root, ctx) {
  let alive = true;
  const view = h('section', { class: 'view', 'aria-labelledby': 'conns-title' });
  root.append(view);

  const addButton = h('button', { type: 'button', class: 'btn primary', text: 'Add a connection', onclick: () => addDialog(ctx, load) });
  const head = h('div', { class: 'view-head' },
    h('div', null,
      h('h1', { class: 'title', id: 'conns-title', text: 'Connections' }),
      h('p', { class: 'subtitle', text: 'Connecting reads the server’s own figures and no key.' })),
    addButton);
  const body = h('div', { class: 'view' }, h('div', { class: 'card' }, emptyState('Reading the saved connections…')));
  view.append(head, body);

  async function load() {
    let data;
    try {
      data = await ctx.api.listConnections();
    } catch (err) {
      if (alive) fill(body, h('div', { class: 'card' }, emptyState(errorText(err), h('button', { type: 'button', class: 'btn', text: 'Try again', onclick: load }))));
      return;
    }
    if (!alive) return;
    refill(body, draw(data), addButton);
  }

  function draw(data) {
    const saved = data.profiles.length
      ? h('div', { class: 'conn-grid' }, data.profiles.map((p) => card(p)))
      : h('div', { class: 'card' }, emptyState(
        'No saved connections yet. Add one: paste a redis:// or rediss:// URL, or a StackExchange.Redis string. The password goes to the OS keychain, never to a file.',
        h('button', { type: 'button', class: 'btn primary', text: 'Add a connection', onclick: () => addDialog(ctx, load) })));
    const lookAgain = h('button', {
      type: 'button', class: 'btn quiet', text: 'Look again',
      onclick: async () => {
        lookAgain.disabled = true;
        try {
          const next = await ctx.api.discoverDocker();
          if (alive) refill(body, draw(next), addButton);
        } catch (err) {
          ctx.notify(errorText(err), 'error');
          lookAgain.disabled = false;
        }
      },
    });
    const docker = data.docker.length
      ? h('div', { class: 'conn-grid' }, data.docker.map((p) => card(p)))
      : h('p', { class: 'muted', text: data.dockerNote || 'Redis and Valkey containers running in Docker show up here by themselves.' });
    return [
      saved,
      h('div', { class: 'section-head' }, h('h2', { class: 'lbl', text: 'Found in Docker' }), lookAgain),
      docker,
    ];
  }

  function card(p) {
    const here = p.status === 'connected' || p.status === 'limited';
    const [glyph, word] = STATUS[p.status] || STATUS['not-open'];
    const address = [p.address || '—', p.tls ? 'TLS' : null, p.docker ? 'Docker' : null].filter(Boolean).join(' · ');
    const access = p.access || (p.readOnly ? 'Read-only' : 'Writes on');

    const main = here
      ? h('button', { type: 'button', class: 'btn main-action', text: 'Disconnect', onclick: () => disconnect() })
      : h('button', { type: 'button', class: 'btn primary', text: p.status === 'connecting' ? 'Connecting…' : 'Connect', disabled: p.status === 'connecting', onclick: () => connect(p, main) });

    return h('article', { class: here ? 'card conn-card on' : 'card conn-card', 'aria-label': p.name },
      h('div', { class: 'conn-top' }, h('h3', { class: 'mono conn-name', text: p.name }), envBadge(p.env)),
      h('span', { class: 'mono conn-address', text: address }),
      h('div', { class: 'conn-meta' },
        h('span', { class: 'conn-status' },
          h('span', { 'aria-hidden': 'true', class: p.status === 'limited' || p.status === 'failed' ? 'glyph' : here ? 'glyph good' : '', text: glyph }),
          h('span', { text: p.statusText ? `${word}: ${p.statusText}` : word })),
        h('span', { text: access }),
        p.signIn ? h('span', { text: `Sign-in: ${p.signIn}` }) : null,
        p.docker && p.container ? h('span', { text: `Container ${p.container}${p.image ? `, image ${p.image}` : ''}` }) : null,
        p.ssh ? h('span', { text: `Through SSH bastion ${p.ssh}` }) : null,
        p.sentinel ? h('span', { text: `Sentinel: ${p.sentinel}` }) : null),
      p.tags.length ? h('div', { class: 'tags', 'aria-label': 'Tags' }, p.tags.map((t) => badge(t))) : null,
      p.providerNotice ? notice(p.providerNotice) : null,
      h('div', { class: 'conn-actions' },
        main,
        h('div', { class: 'conn-more' },
          h('button', { type: 'button', class: 'btn quiet', text: 'Test', 'aria-label': `Test ${p.name}`, onclick: () => testDialog(ctx, p) }),
          h('button', { type: 'button', class: 'btn quiet', text: 'Edit', 'aria-label': `Edit ${p.name}`, onclick: () => editDialog(ctx, p, load) }),
          p.docker ? null : h('button', { type: 'button', class: 'btn quiet', text: 'Remove', 'aria-label': `Remove ${p.name}`, onclick: () => remove(p) }))));
  }

  async function connect(p, button) {
    button.disabled = true;
    button.textContent = 'Connecting…';
    try {
      const state = await ctx.api.connect(p.name);
      if (state.connected) {
        ctx.notify(`Connected to ${p.name}. No key was read.`, 'ok');
        ctx.navigate('#/overview');
      }
    } catch (err) {
      ctx.notify(errorText(err), err.cancelled ? 'info' : 'error');
      if (alive) load();
    }
  }

  async function disconnect() {
    try {
      await ctx.api.disconnect();
    } catch (err) {
      ctx.notify(errorText(err), 'error');
    }
    if (alive) load();
  }

  async function remove(p) {
    const answer = await confirmDialog({
      kind: 'ask', title: `Remove ${p.name}?`, yes: 'Remove',
      text: 'The saved connection and its entry in the OS keychain are deleted. Nothing on the server changes.',
    });
    if (!answer) return;
    try {
      await ctx.api.removeConnection(p.name);
      ctx.notify(`Removed ${p.name}.`, 'ok');
    } catch (err) {
      ctx.notify(errorText(err), err.cancelled ? 'info' : 'error');
    }
    if (alive) load();
  }

  load();
  return () => {
    alive = false;
  };
}

// ---------- Test ----------

function testDialog(ctx, p) {
  openDialog({
    title: `Test ${p.name}`,
    build(close) {
      const box = h('div', { class: 'form', 'aria-live': 'polite' }, h('p', { class: 'muted', text: 'Connecting and checking what this user may do…' }));
      const done = h('div', { class: 'dialog-actions' }, h('button', { type: 'button', class: 'btn', text: 'Close', 'data-autofocus': true, onclick: () => close(null) }));
      ctx.api.testConnection(p.name).then(
        (r) => {
          fill(box,
            notice(r.summary || (r.ok ? 'Every check passed.' : 'Some checks were refused.'), r.ok ? 'ok' : 'warn'),
            r.checks.length ? h('ul', { class: 'list' }, r.checks.map((c) => h('li', { class: 'row row-check' },
              h('span', { 'aria-hidden': 'true', class: c.ok ? 'glyph good' : 'glyph', text: c.ok ? '✓' : '✗' }),
              h('span', null,
                h('span', { class: 'mono name', text: c.name }),
                h('span', { class: 'sr-only', text: c.ok ? ' allowed' : ' off' }),
                c.why ? h('span', { class: 'sub', text: c.why }) : null)))) : null,
            r.notes.map((n) => h('p', { class: 'muted', text: n })));
        },
        (err) => fill(box, notice(errorText(err))),
      );
      return [box, done];
    },
  });
}

// ---------- Edit ----------

function editDialog(ctx, p, reload) {
  openDialog({
    title: `Edit ${p.name}`,
    build(close) {
      const envId = nextId('env');
      const tagsId = nextId('tags');
      const writesId = nextId('writes');
      const env = h('select', { class: 'field', id: envId }, ENVS.map((e) => h('option', { value: e, text: e, selected: e === p.env })));
      const tags = h('input', { class: 'field', type: 'text', id: tagsId, value: p.tags.join(', '), autocomplete: 'off', spellcheck: false });
      const writes = h('input', { type: 'checkbox', id: writesId, checked: !p.readOnly });
      const hint = h('p', { class: 'hint', id: `${writesId}-hint` });
      writes.setAttribute('aria-describedby', hint.id);
      const error = h('p', { class: 'form-error', role: 'alert' });
      const save = h('button', { type: 'submit', class: 'btn primary', text: 'Save' });

      const sync = () => {
        const prod = env.value === 'PROD';
        if (!writes.checked) {
          hint.textContent = prod
            ? 'Read-only, the default for production: redis-lens never sends a write to this connection.'
            : 'Read-only: redis-lens never sends a write to this connection.';
        } else if (p.readOnly) {
          hint.textContent = prod
            ? `This is a production connection. Turning writes on asks you to type its name. ${WRITES_HINT}`
            : `Turning writes on asks first. ${WRITES_HINT}`;
        } else {
          hint.textContent = `Writes are on. ${WRITES_HINT} Turning them off never asks.`;
        }
      };
      env.addEventListener('change', sync);
      writes.addEventListener('change', sync);
      sync();

      return h('form', {
        class: 'form',
        onsubmit: async (e) => {
          e.preventDefault();
          const changes = {};
          const newTags = parseTags(tags.value);
          if (!p.docker && env.value !== p.env) changes.env = env.value;
          if (!p.docker && newTags.join('\n') !== p.tags.join('\n')) changes.tags = newTags;
          if (writes.checked === p.readOnly) changes.writes = writes.checked;
          if (!Object.keys(changes).length) {
            close(null);
            return;
          }
          save.disabled = true;
          error.textContent = '';
          try {
            await ctx.api.updateConnection(p.name, changes);
            ctx.notify(`Saved ${p.name}.`, 'ok');
            close(true);
            reload();
          } catch (err) {
            error.textContent = errorText(err);
            save.disabled = false;
          }
        },
      },
      p.docker
        ? h('p', { class: 'muted', text: 'A Docker container has no saved profile, so this lasts for the session only.' })
        : [
          h('div', { class: 'form-row' }, h('label', { htmlFor: envId, text: 'Environment' }), env),
          h('div', { class: 'form-row' }, h('label', { htmlFor: tagsId, text: 'Tags' }), tags, h('p', { class: 'hint', text: 'Separate tags with commas.' })),
        ],
      h('div', { class: 'form-row' },
        h('div', { class: 'switch' }, writes, h('label', { htmlFor: writesId, text: 'Allow writes' })),
        hint),
      error,
      h('div', { class: 'dialog-actions' },
        h('button', { type: 'button', class: 'btn', text: 'Cancel', onclick: () => close(null) }),
        save));
    },
  });
}

// ---------- Add ----------

/** A guess at a name from the address, as the TUI's add flow does: the host's first label. */
export function guessName(url) {
  const s = String(url ?? '').trim().replace(/^rediss?:\/\//i, '');
  const authority = s.split(/[/?#,]/)[0];
  const hostPort = authority.slice(authority.lastIndexOf('@') + 1);
  const m = /^\[?([^\]]*?)\]?(?::(\d+))?$/.exec(hostPort);
  if (!m || !m[1]) return '';
  const host = m[1].toLowerCase();
  if (host === 'localhost' || /^[\d.]+$/.test(host) || host.includes(':')) return `local-${m[2] || '6379'}`;
  return host.split('.')[0].replace(/[^a-z0-9_-]/g, '-');
}

function addDialog(ctx, reload) {
  let password = null;
  let sentinelPassword = null;
  openDialog({
    title: 'Add a connection',
    wide: true,
    onClose() {
      // Whatever closed the dialog, the secrets leave the page with it.
      if (password) password.value = '';
      if (sentinelPassword) sentinelPassword.value = '';
    },
    build(close) {
      const errors = new Map();
      const input = (name, label, props = {}, hintText = '') => {
        const id = nextId(name);
        const el = h('input', { class: 'field', type: 'text', id, autocomplete: 'off', spellcheck: false, ...props });
        const err = h('p', { class: 'field-error', id: `${id}-err` });
        const hint = hintText ? h('p', { class: 'hint', id: `${id}-hint`, text: hintText }) : null;
        el.setAttribute('aria-describedby', [hint && hint.id, err.id].filter(Boolean).join(' '));
        errors.set(name, { el, err });
        return { el, hint, row: h('div', { class: 'form-row' }, h('label', { htmlFor: id, text: label }), el, hint, err) };
      };
      const select = (name, label, options) => {
        const id = nextId(name);
        const el = h('select', { class: 'field', id }, options.map(([value, text]) => h('option', { value, text })));
        return { el, row: h('div', { class: 'form-row' }, h('label', { htmlFor: id, text: label }), el) };
      };
      const checkbox = (name, label, hintText) => {
        const id = nextId(name);
        const el = h('input', { type: 'checkbox', id });
        return { el, row: h('div', { class: 'check' }, el, h('label', { htmlFor: id }, label, h('span', { class: 'hint', text: hintText }))) };
      };

      const url = input('url', 'Address', { class: 'field mono', placeholder: 'rediss://host:6379', 'data-autofocus': true },
        'A redis:// or rediss:// URL, or a host:port,ssl=True string. A password pasted here moves to the password box.');
      const name = input('name', 'Name', { class: 'field mono' });
      const env = select('env', 'Environment', [['', 'Guess from the address']].concat(ENVS.map((e) => [e, e])));
      const pass = input('password', 'Password', { type: 'password' }, 'Goes to the OS keychain, never to a file. Leave it empty for a server without one.');
      password = pass.el;
      const tags = input('tags', 'Tags', {}, 'Separate tags with commas.');
      const writes = select('writes', 'Writes', [
        ['', 'Default for the environment'],
        ['off', 'Read-only'],
        ['on', 'Writes on'],
      ]);
      const writesHint = h('p', { class: 'hint', text: `By default production is read-only and everything else has writes on. ${WRITES_HINT}` });
      writes.row.append(writesHint);

      const tls = checkbox('tls', 'Use TLS', 'rediss:// and ssl=True turn it on by themselves.');
      const provider = select('provider', 'Provider', PROVIDERS);
      const sshHost = input('sshHost', 'Bastion', { class: 'field mono', placeholder: '[user@]host[:port]' }, 'The user goes to the OS keychain, never to the config file.');
      const sshIdentity = input('sshIdentity', 'Identity file', { class: 'field mono', placeholder: '~/.ssh/id_ed25519' });
      const sshKnownHosts = input('sshKnownHosts', 'Known hosts file', { class: 'field mono' });
      const sentinels = input('sentinelAddresses', 'Sentinel addresses', { class: 'field mono', placeholder: 'host:26379, host:26379' });
      const sentinelMaster = input('sentinelMaster', 'Master name', { class: 'field mono', placeholder: 'mymaster' });
      const sentinelPass = input('sentinelPassword', 'Sentinel password', { type: 'password' }, 'Only when the Sentinels have their own. It goes to the OS keychain.');
      sentinelPassword = sentinelPass.el;
      const token = input('tokenCommand', 'Token command', { class: 'field mono', placeholder: 'program --flag value' },
        'A program that prints a token. It runs again at every connect, so redis-lens shows it back and asks before saving it.');
      const preferReplica = checkbox('preferReplica', 'Prefer a replica', 'An analysis reads a verified replica when there is one.');
      const forceStandalone = checkbox('forceStandalone', 'Force standalone', 'Read a cluster node as a single server.');

      let nameTouched = false;
      let tlsTouched = false;
      name.el.addEventListener('input', () => {
        nameTouched = true;
      });
      tls.el.addEventListener('change', () => {
        tlsTouched = true;
      });
      url.el.addEventListener('input', () => {
        const split = splitUrlSecret(url.el.value);
        if (split.password !== null) {
          url.el.value = split.url;
          pass.el.value = split.password;
          url.hint.textContent = 'The password moved to the password box, so it is not readable here.';
        }
        if (!nameTouched) name.el.value = guessName(url.el.value);
        if (!tlsTouched) tls.el.checked = urlWantsTls(url.el.value);
      });

      const advanced = h('details', { class: 'advanced' },
        h('summary', { text: 'Advanced' }),
        h('div', { class: 'form' },
          h('fieldset', { class: 'group' }, h('legend', { text: 'TLS and provider' }), tls.row, provider.row),
          h('fieldset', { class: 'group' }, h('legend', { text: 'SSH tunnel' }), sshHost.row, h('div', { class: 'form-grid' }, sshIdentity.row, sshKnownHosts.row)),
          h('fieldset', { class: 'group' }, h('legend', { text: 'Sentinel' }), h('div', { class: 'form-grid' }, sentinels.row, sentinelMaster.row), sentinelPass.row),
          h('fieldset', { class: 'group' }, h('legend', { text: 'Token command' }), token.row),
          h('fieldset', { class: 'group' }, h('legend', { text: 'Reading' }), preferReplica.row, forceStandalone.row)));

      const formError = h('p', { class: 'form-error', role: 'alert' });
      const save = h('button', { type: 'submit', class: 'btn primary', text: 'Add connection' });

      function showErrors(found) {
        let first = null;
        for (const [field, { el, err }] of errors) {
          const message = found[field] || '';
          err.textContent = message;
          if (message) {
            el.setAttribute('aria-invalid', 'true');
            if (!first) first = el;
          } else el.removeAttribute('aria-invalid');
        }
        if (first) {
          if (advanced.contains(first)) advanced.open = true;
          first.focus();
        }
        return !first;
      }

      async function submit() {
        formError.textContent = '';
        const found = checkAddForm({
          url: url.el.value, name: name.el.value, tokenCommand: token.el.value, hasPassword: pass.el.value.length > 0,
          sshHost: sshHost.el.value, sshIdentity: sshIdentity.el.value, sshKnownHosts: sshKnownHosts.el.value,
          sentinelAddresses: sentinels.el.value, sentinelMaster: sentinelMaster.el.value, hasSentinelPassword: sentinelPass.el.value.length > 0,
        });
        if (!showErrors(found)) return;

        const tokenArgs = token.el.value.trim() ? splitArgs(token.el.value).args : [];
        let tokenConfirmed = false;
        if (tokenArgs.length) {
          const answer = await confirmDialog({
            kind: 'ask', title: 'Save this token command?', yes: 'Save it',
            text: 'redis-lens will run this command at every connect, test and analysis, one argument per line:',
            code: tokenArgs.join('\n'),
            after: 'It is saved in the config file, which gets copied around. What it prints is a credential: it is never saved or shown.',
          });
          if (!answer) return;
          tokenConfirmed = true;
        }

        save.disabled = true;
        // The one place the passwords are read: straight into the request.
        let form = {
          url: url.el.value.trim(), name: name.el.value.trim(), env: env.el.value,
          password: pass.el.value, tags: parseTags(tags.el.value),
          writes: writes.el.value === 'on' ? true : writes.el.value === 'off' ? false : null,
          tls: tls.el.checked, provider: provider.el.value,
          sshHost: sshHost.el.value.trim(), sshIdentity: sshIdentity.el.value.trim(), sshKnownHosts: sshKnownHosts.el.value.trim(),
          sentinelAddresses: parseTags(sentinels.el.value), sentinelMaster: sentinelMaster.el.value.trim(), sentinelPassword: sentinelPass.el.value,
          tokenCommand: tokenArgs, tokenCommandConfirmed: tokenConfirmed,
          preferReplica: preferReplica.el.checked, forceStandalone: forceStandalone.el.checked,
        };
        const secretInUrl = urlHasSecret(form.url);
        const hadSecret = secretInUrl || form.password.length > 0 || form.sentinelPassword.length > 0;
        try {
          const added = await ctx.api.addConnection(form);
          ctx.notify(`Saved ${added.name || form.name}. Connecting asks what this user may do before it reads anything.`, 'ok');
          close(true);
          reload();
        } catch (err) {
          formError.textContent = hadSecret
            ? `${errorText(err)} The password was cleared from this page: type it again to retry.`
            : errorText(err);
          save.disabled = false;
        } finally {
          pass.el.value = '';
          sentinelPass.el.value = '';
          if (secretInUrl) url.el.value = '';
          form = null;
        }
      }

      return h('form', {
        class: 'form',
        autocomplete: 'off',
        onsubmit: (e) => {
          e.preventDefault();
          submit();
        },
      },
      url.row,
      h('div', { class: 'form-grid' }, name.row, env.row),
      pass.row,
      h('div', { class: 'form-grid' }, tags.row, writes.row),
      advanced,
      formError,
      h('div', { class: 'dialog-actions' },
        h('button', { type: 'button', class: 'btn', text: 'Cancel', onclick: () => close(null) }),
        save));
    },
  });
}
