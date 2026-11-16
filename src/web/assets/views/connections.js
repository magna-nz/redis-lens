// Connections: the home screen. Saved connections and the Redis or Valkey containers
// found in Docker, as cards; connect, test, edit, remove, and the add dialog.
//
// Rule 1: no credential is ever drawn here, because none is ever sent to the page: a
// connection says only where its credentials live. The password boxes in the add dialog
// are read once, at submit, straight into the request body, and emptied as soon as the
// request returns (success or not). They are never logged, never put in the address or in
// localStorage, and never echoed back.

import {
  h, fill, refill, badge, envBadge, emptyState, notice, openDialog, confirmDialog, errorText,
  parseTags, splitArgs, splitUrlSecret, urlHasSecret, checkAddForm,
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

/** What Docker discovery last found. Discovery pings every container, so it runs when
 *  the page first shows Connections and when "Look again" is pressed, not on every redraw. */
let dockerFound = null;
let dockerAsking = null;

/** What Docker discovery found, asked once and kept: Connections and the top bar's
 *  connection menu share it, so opening the menu pings no container that was pinged
 *  already. `again` asks Docker again ("Look again"). */
export function loadDocker(api, again = false) {
  if (dockerFound && !again) return Promise.resolve(dockerFound);
  if (dockerAsking && !again) return dockerAsking;
  const asking = api.listDocker().then(
    (found) => {
      if (dockerAsking === asking) dockerAsking = null;
      dockerFound = found;
      return found;
    },
    (err) => {
      if (dockerAsking === asking) dockerAsking = null;
      throw err;
    },
  );
  dockerAsking = asking;
  return asking;
}

/** What the last discovery found, without asking (null before the first). */
export function dockerKnown() {
  return dockerFound;
}

let fieldSeq = 0;
const nextId = (name) => `f-${name}-${(fieldSeq += 1)}`;

/** What a card says about one connection right now, from the server's state. `limited`
 *  is whether the permission check of the connected one refused something. */
export function profileStatus(p, state, limited = false) {
  const c = state.conn;
  if (!c || c.name !== p.name || c.docker !== p.docker) return { status: 'not-open', text: null };
  if (state.connecting) return { status: 'connecting', text: null };
  if (state.failed) return { status: 'failed', text: state.failed };
  if (!state.connected) return { status: 'not-open', text: null };
  if (state.lost) return { status: 'failed', text: state.lost };
  return { status: limited ? 'limited' : 'connected', text: limited ? 'some checks were refused, so the panels that need them are off' : null };
}

/** Why writes are on or off, in words. `refused` counts the writes the server refused
 *  this session (the connected one only). */
export function accessText(p, refused = 0) {
  if (p.readOnly) return p.env === 'PROD' ? 'Read-only, the default for production' : 'Read-only';
  const why = refused > 0 ? `${refused} refused by the server` : p.env === 'PROD' ? 'each one asks first' : 'the default outside production';
  return `Writes on (${why})`;
}

/** The tags to add and to remove so `before` becomes `after`. */
export function tagChanges(before, after) {
  return { addTags: after.filter((t) => !before.includes(t)), removeTags: before.filter((t) => !after.includes(t)) };
}

export function mount(root, ctx) {
  let alive = true;
  let limited = false;
  let profiles = null;
  const view = h('section', { class: 'view', 'aria-labelledby': 'conns-title' });
  root.append(view);

  const addButton = h('button', { type: 'button', class: 'btn primary', text: 'Add a connection', onclick: () => addDialog(ctx, load) });
  const head = h('div', { class: 'view-head' },
    h('div', null,
      h('h1', { class: 'title', id: 'conns-title', text: 'Connections' }),
      h('p', { class: 'subtitle', text: 'Connecting reads the server’s own figures and no key.' })),
    addButton);
  const savedBox = h('div', null, h('div', { class: 'card' }, emptyState('Reading the saved connections…')));
  const dockerBox = h('div', null, h('p', { class: 'muted', text: 'Looking for Redis and Valkey containers in Docker…' }));
  const lookAgain = h('button', { type: 'button', class: 'btn quiet', text: 'Look again', onclick: () => drawDocker(true) });
  const host = (typeof window !== 'undefined' && window.location && window.location.host) || '127.0.0.1';
  view.append(head, savedBox,
    h('div', { class: 'section-head' }, h('h2', { class: 'lbl', text: 'Found in Docker' }), lookAgain),
    dockerBox,
    h('p', { class: 'muted small' }, 'Served by the redis-lens running in your terminal at ', h('span', { class: 'mono', text: host }), '. Only this computer can open it.'));

  async function load() {
    const state = ctx.state();
    let data;
    try {
      data = await ctx.api.listConnections();
      // The permission check of the connected one says whether it is limited. Overview
      // answers from what the connect already read: no command goes to the server.
      limited = false;
      if (state.connected) {
        try {
          limited = (await ctx.api.getOverview()).checks.some((c) => c.status === 'denied');
        } catch {
          limited = false;
        }
      }
    } catch (err) {
      if (alive) fill(savedBox, h('div', { class: 'card' }, emptyState(errorText(err), h('button', { type: 'button', class: 'btn', text: 'Try again', onclick: load }))));
      return;
    }
    if (!alive) return;
    profiles = data.profiles;
    refill(savedBox, profiles.length
      ? h('div', { class: 'conn-grid' }, profiles.map((p) => card(p)))
      : h('div', { class: 'card' }, emptyState(
        'No saved connections yet. Add one: paste a redis:// or rediss:// URL, or a StackExchange.Redis string. The password goes to the OS keychain, never to a file.',
        h('button', { type: 'button', class: 'btn primary', text: 'Add a connection', onclick: () => addDialog(ctx, load) }))), addButton);
  }

  async function drawDocker(again = false) {
    lookAgain.disabled = true;
    try {
      const found = await loadDocker(ctx.api, again);
      if (!alive) return;
      refill(dockerBox, found.containers.length
        ? h('div', { class: 'conn-grid' }, found.containers.map((p) => card(p)))
        : h('p', { class: 'muted', text: 'No Redis or Valkey container is running. They show up here by themselves.' }), lookAgain);
    } catch (err) {
      if (alive) fill(dockerBox, notice(`Docker could not be asked: ${errorText(err)}`));
    }
    lookAgain.disabled = false;
  }

  function card(p) {
    const state = ctx.state();
    const { status, text } = profileStatus(p, state, limited);
    const here = status === 'connected' || status === 'limited';
    const mine = status !== 'not-open';
    const conn = mine ? state.conn : null;
    const [glyph, word] = STATUS[status];
    const address = [p.address || '—', p.tls ? 'TLS' : null, p.db ? `db ${p.db}` : null, p.docker ? 'Docker' : null].filter(Boolean).join(' · ');
    const serverLine = here && conn ? [[conn.server, conn.version].filter(Boolean).join(' '), conn.topology, conn.target ? `reading ${conn.target}` : null].filter(Boolean).join(' · ') : null;

    const main = mine && status !== 'failed'
      ? h('button', { type: 'button', class: 'btn main-action', text: status === 'connecting' ? 'Cancel' : 'Disconnect', onclick: () => disconnect() })
      : h('button', { type: 'button', class: 'btn primary', text: status === 'failed' ? 'Connect again' : 'Connect', onclick: () => connect(p, main) });

    return h('article', { class: here ? 'card conn-card on' : 'card conn-card', 'aria-label': p.name },
      h('div', { class: 'conn-top' }, h('h3', { class: 'mono conn-name', text: p.name }), envBadge(p.env)),
      h('span', { class: 'mono conn-address', text: address }),
      h('div', { class: 'conn-meta' },
        h('span', { class: 'conn-status' },
          h('span', { 'aria-hidden': 'true', class: status === 'failed' ? 'glyph bad' : status === 'limited' ? 'glyph' : here ? 'glyph good' : '', text: glyph }),
          h('span', { text: text ? `${word}: ${text}` : word })),
        serverLine ? h('span', { text: serverLine }) : null,
        conn && conn.via ? h('span', { text: conn.via }) : null,
        p.docker
          ? (here && conn ? h('span', { text: accessText(conn, conn.refused.length) }) : null)
          : h('span', { text: accessText(here && conn ? conn : p, here && conn ? conn.refused.length : 0) }),
        p.signIn ? h('span', { text: `Sign-in: ${p.signIn}` }) : null,
        p.docker && p.image ? h('span', { text: `Image ${p.image}` }) : null,
        p.provider ? h('span', { text: `Provider: ${p.provider}` }) : null,
        p.ssh ? h('span', { text: `Through SSH bastion ${p.ssh}` }) : null,
        p.sentinel ? h('span', { text: `Sentinel: ${p.sentinel}${p.sentinelTls ? ' · TLS' : ''}` }) : null,
        p.preferReplica ? h('span', { text: 'An analysis reads a replica when there is one' }) : null,
        p.standalone ? h('span', { text: 'Read as a standalone server' }) : null),
      p.tags.length ? h('div', { class: 'tags', 'aria-label': 'Tags' }, p.tags.map((t) => badge(t))) : null,
      p.providerNotice ? notice(p.providerNotice) : null,
      h('div', { class: 'conn-actions' },
        main,
        p.docker && here && conn ? h('div', { class: 'conn-more' },
          h('button', {
            type: 'button', class: 'btn quiet', text: conn.readOnly ? 'Turn writes on' : 'Turn writes off',
            'aria-label': `${conn.readOnly ? 'Turn writes on' : 'Turn writes off'} for ${p.name}, for this session`,
            onclick: () => sessionWrites(conn.readOnly),
          })) : null,
        p.docker ? null : h('div', { class: 'conn-more' },
          h('button', { type: 'button', class: 'btn quiet', text: 'Test', 'aria-label': `Test ${p.name}`, onclick: () => testDialog(ctx, p) }),
          h('button', { type: 'button', class: 'btn quiet', text: 'Edit', 'aria-label': `Edit ${p.name}`, onclick: () => editDialog(ctx, p, load) }),
          h('button', { type: 'button', class: 'btn quiet danger', text: 'Remove', 'aria-label': `Remove ${p.name}`, onclick: () => remove(p) }))));
  }

  function connect(p, button) {
    button.disabled = true;
    button.textContent = 'Connecting…';
    ctx.connect(p.docker ? { docker: p.container } : { name: p.name });
  }

  /** A container has no saved profile: its writes switch is the session's. */
  async function sessionWrites(on) {
    try {
      const done = await ctx.api.setSessionWrites(on);
      ctx.notify(done.message || (on ? 'Writes are on for this session.' : 'Writes are off for this session.'), 'ok');
    } catch (err) {
      ctx.notify(errorText(err), err.cancelled ? 'info' : 'error');
    }
    if (alive) load();
    if (alive) drawDocker();
  }

  async function disconnect() {
    try {
      await ctx.api.disconnect();
    } catch (err) {
      ctx.notify(errorText(err), 'error');
    }
  }

  async function remove(p) {
    const answer = await confirmDialog({
      kind: 'ask', title: `Remove ${p.name}?`, yes: 'Remove',
      text: 'The saved connection and its entries in the OS keychain are deleted. Nothing on the server changes.',
    });
    if (!answer) return;
    try {
      // Removing the connection the session is on closes it: the state goes back to idle.
      const done = await ctx.api.removeConnection(p.name);
      ctx.notify(done.message || `Removed ${p.name}.`, 'ok');
    } catch (err) {
      ctx.notify(errorText(err), err.cancelled ? 'info' : 'error');
    }
    if (alive) load();
  }

  load();
  drawDocker();
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
      const word = { allowed: 'allowed', denied: 'refused', skipped: 'not checked' };
      const glyph = { allowed: '✓', denied: '✗', skipped: '–' };
      ctx.api.testConnection(p.name).then(
        (r) => {
          fill(box,
            notice(r.summary, r.ok ? 'ok' : 'warn'),
            r.checks.length ? h('ul', { class: 'list' }, r.checks.map((c) => h('li', { class: 'row row-check' },
              h('span', { 'aria-hidden': 'true', class: c.status === 'allowed' ? 'glyph good' : c.status === 'denied' ? 'glyph bad' : 'muted', text: glyph[c.status] }),
              h('span', null,
                h('span', { class: 'mono name', text: c.name }),
                h('span', { class: 'sr-only', text: ` ${word[c.status]}` }),
                c.why ? h('span', { class: 'sub', text: c.why }) : null)))) : null,
            r.notes.map((n) => h('p', { class: 'muted', text: n })));
        },
        (err) => fill(box, notice(`The test could not connect: ${errorText(err)}`)),
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
      const tagsId = nextId('tags');
      const writesId = nextId('writes');
      const standaloneId = nextId('standalone');
      const tags = h('input', { class: 'field', type: 'text', id: tagsId, value: p.tags.join(', '), autocomplete: 'off', spellcheck: false });
      const writes = h('input', { type: 'checkbox', id: writesId, checked: !p.readOnly });
      const standalone = h('input', { type: 'checkbox', id: standaloneId, checked: p.standalone });
      const hint = h('p', { class: 'hint', id: `${writesId}-hint` });
      writes.setAttribute('aria-describedby', hint.id);
      const error = h('p', { class: 'form-error', role: 'alert' });
      const save = h('button', { type: 'submit', class: 'btn primary', text: 'Save' });
      const prod = p.env === 'PROD';

      const sync = () => {
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
      writes.addEventListener('change', sync);
      sync();

      return h('form', {
        class: 'form',
        onsubmit: async (e) => {
          e.preventDefault();
          const changes = tagChanges(p.tags, parseTags(tags.value));
          if (writes.checked === p.readOnly) changes.writes = writes.checked;
          if (standalone.checked !== p.standalone) changes.standalone = standalone.checked;
          if (!changes.addTags.length && !changes.removeTags.length && changes.writes === undefined && changes.standalone === undefined) {
            close(null);
            return;
          }
          save.disabled = true;
          error.textContent = '';
          let savedPart = false;
          try {
            // The writes of the connection the session is on are the session's switch (the
            // TUI's w); everything else, and any other connection, is the saved profile's.
            const c = ctx.state().conn;
            const mine = !!c && !c.docker && c.name === p.name;
            const { writes: wanted, ...rest } = changes;
            const saved = mine ? rest : changes;
            let message = null;
            if (saved.addTags.length || saved.removeTags.length || saved.writes !== undefined || saved.standalone !== undefined) {
              message = (await ctx.api.updateConnection(p.name, saved)).message;
              savedPart = true;
            }
            if (mine && wanted !== undefined) message = (await ctx.api.setSessionWrites(wanted)).message;
            ctx.notify(message || `Saved ${p.name}.`, 'ok');
            close(true);
            reload();
          } catch (err) {
            // The tags or the standalone switch may be saved already while the writes
            // question was cancelled or refused: say so, and show the card as it now is.
            if (savedPart) {
              ctx.notify(`Saved ${p.name}, but not its writes: ${errorText(err)}`, err.cancelled ? 'info' : 'error');
              close(true);
              reload();
              return;
            }
            error.textContent = errorText(err);
            save.disabled = false;
          }
        },
      },
      h('p', { class: 'muted' }, 'Environment ', envBadge(p.env), ' To change the environment, add the connection again with Replace ticked.'),
      h('div', { class: 'form-row' }, h('label', { htmlFor: tagsId, text: 'Tags' }), tags, h('p', { class: 'hint', text: 'Separate tags with commas.' })),
      h('div', { class: 'form-row' },
        h('div', { class: 'switch' }, writes, h('label', { htmlFor: writesId, text: 'Allow writes' })),
        hint),
      h('div', { class: 'check' }, standalone, h('label', { htmlFor: standaloneId }, 'Force standalone', h('span', { class: 'hint', text: 'Read a cluster node as a single server.' }))),
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
  const s = String(url ?? '').trim().replace(/^(?:rediss?|valkeys?):\/\//i, '');
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
  const features = ctx.state().features;
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
        const err = h('p', { class: 'field-error', id: `${id}-err` });
        errors.set(name, { el, err });
        return { el, row: h('div', null, h('div', { class: 'check' }, el, h('label', { htmlFor: id }, label, h('span', { class: 'hint', text: hintText }))), err) };
      };

      const url = input('url', 'Address', { class: 'field mono', placeholder: 'rediss://host:6379', 'data-autofocus': true },
        'A redis:// or valkey:// URL (rediss:// or valkeys:// for TLS), or a host:port,ssl=True string. A password pasted here moves to the password box.');
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
      writes.row.append(h('p', { class: 'hint', text: `By default production is read-only and everything else has writes on. ${WRITES_HINT}` }));

      const provider = select('provider', 'Provider', PROVIDERS);
      const sshHost = input('sshHost', 'Bastion', { class: 'field mono', placeholder: '[user@]host[:port]' }, 'The user goes to the OS keychain, never to the config file.');
      const sshIdentity = input('sshIdentity', 'Identity file', { class: 'field mono', placeholder: '~/.ssh/id_ed25519' });
      const sshKnownHosts = input('sshKnownHosts', 'Known hosts file', { class: 'field mono' });
      const sentinels = input('sentinelAddresses', 'Sentinel addresses', { class: 'field mono', placeholder: 'host:26379, host:26379' });
      const sentinelMaster = input('sentinelMaster', 'Master name', { class: 'field mono', placeholder: 'mymaster' });
      const sentinelUser = input('sentinelUser', 'Sentinel username', {}, 'Only when the Sentinels have their own. It goes to the OS keychain.');
      const sentinelPass = input('sentinelPassword', 'Sentinel password', { type: 'password' }, 'Only when the Sentinels have their own. It goes to the OS keychain.');
      sentinelPassword = sentinelPass.el;
      const sentinelTls = checkbox('sentinelTls', 'Sentinels use TLS', 'Connect to the Sentinels over TLS.');
      const token = input('tokenCommand', 'Token command', { class: 'field mono', placeholder: 'program --flag value' },
        'A program that prints a token. It runs again at every connect, directly and not through a shell, so redis-lens shows it back and asks before saving it.');
      const entra = features.entraId ? checkbox('entra', 'Sign in with Microsoft Entra ID', 'Uses your developer tools (az login). No password is stored.') : null;
      const preferReplica = checkbox('preferReplica', 'Prefer a replica', 'An analysis reads a verified replica when there is one.');
      const standalone = checkbox('standalone', 'Force standalone', 'Read a cluster node as a single server.');
      const force = checkbox('force', 'Replace', 'Overwrite a saved connection of the same name. Without this, a name that is already saved is refused.');

      // A message under a box goes as soon as the box is changed.
      for (const { el, err } of errors.values()) {
        const clear = () => {
          err.textContent = '';
          el.removeAttribute('aria-invalid');
        };
        el.addEventListener('input', clear);
        el.addEventListener('change', clear);
      }

      let nameTouched = false;
      name.el.addEventListener('input', () => {
        nameTouched = true;
      });
      url.el.addEventListener('input', () => {
        const split = splitUrlSecret(url.el.value);
        if (split.password !== null) {
          url.el.value = split.url;
          pass.el.value = split.password;
          url.hint.textContent = 'The password moved to the password box, so it is not readable here.';
        }
        if (!nameTouched) name.el.value = guessName(url.el.value);
      });

      const advanced = h('details', { class: 'advanced' },
        h('summary', { text: 'Advanced' }),
        h('div', { class: 'form' },
          h('fieldset', { class: 'group' }, h('legend', { text: 'Provider' }), provider.row),
          h('fieldset', { class: 'group' }, h('legend', { text: 'SSH tunnel' }), sshHost.row, h('div', { class: 'form-grid' }, sshIdentity.row, sshKnownHosts.row)),
          h('fieldset', { class: 'group' }, h('legend', { text: 'Sentinel' }),
            h('div', { class: 'form-grid' }, sentinels.row, sentinelMaster.row),
            h('div', { class: 'form-grid' }, sentinelUser.row, sentinelPass.row),
            sentinelTls.row),
          h('fieldset', { class: 'group' }, h('legend', { text: 'Other ways to sign in' }), token.row, entra ? entra.row : null),
          h('fieldset', { class: 'group' }, h('legend', { text: 'Reading' }), preferReplica.row, standalone.row),
          h('fieldset', { class: 'group' }, h('legend', { text: 'Saving' }), force.row)));

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
          url: url.el.value, name: name.el.value, tokenCommand: token.el.value, hasPassword: pass.el.value.length > 0, entra: !!entra && entra.el.checked,
          sshHost: sshHost.el.value, sshIdentity: sshIdentity.el.value, sshKnownHosts: sshKnownHosts.el.value,
          sentinelAddresses: sentinels.el.value, sentinelMaster: sentinelMaster.el.value, sentinelUser: sentinelUser.el.value,
          sentinelTls: sentinelTls.el.checked, hasSentinelPassword: sentinelPass.el.value.length > 0,
        });
        if (!showErrors(found)) return;

        save.disabled = true;
        // The one place the passwords are read: straight into the request. A question the
        // server asks before saving (a token command, writes on production) is answered
        // by its id: the passwords are not sent a second time.
        let form = {
          url: url.el.value.trim(), name: name.el.value.trim(), env: env.el.value,
          password: pass.el.value, tags: parseTags(tags.el.value),
          writes: writes.el.value === 'on' ? true : writes.el.value === 'off' ? false : null,
          provider: provider.el.value,
          sshHost: sshHost.el.value.trim(), sshIdentity: sshIdentity.el.value.trim(), sshKnownHosts: sshKnownHosts.el.value.trim(),
          sentinelAddresses: parseTags(sentinels.el.value), sentinelMaster: sentinelMaster.el.value.trim(), sentinelUser: sentinelUser.el.value.trim(),
          sentinelPassword: sentinelPass.el.value, sentinelTls: sentinelTls.el.checked,
          tokenCommand: token.el.value.trim() ? splitArgs(token.el.value).args : [],
          entra: !!entra && entra.el.checked,
          preferReplica: preferReplica.el.checked, standalone: standalone.el.checked, force: force.el.checked,
        };
        const secretInUrl = urlHasSecret(form.url);
        const hadSecret = secretInUrl || form.password.length > 0 || form.sentinelPassword.length > 0;
        try {
          const added = await ctx.api.addConnection(form);
          ctx.notify(added.message || `Saved ${added.name || form.name}.`, 'ok');
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
