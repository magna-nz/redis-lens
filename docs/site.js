/* redis-lens docs: syntax highlighting, copy buttons, screen tabs, the mobile
   nav, the active section in the nav and the Terminal / Dashboard switch. Everything here is progressive
   enhancement: with JavaScript off the page is complete, code is plain text,
   every terminal screen is shown and the nav still links to every anchor. */
(function () {
  'use strict';

  /* ---------------- syntax highlighting ---------------- */

  var RULES = {
    bash: [
      { re: /#[^\n]*/y, cls: 'tok-comment' },
      { re: /"(?:[^"\\\n]|\\.)*"/y, cls: 'tok-string' },
      { re: /'[^'\n]*'/y, cls: 'tok-string' },
      { re: /\b(?:redis-lens|cargo|make|docker|printf|redis-cli|az|gcloud|ssh)\b/y, cls: 'tok-keyword' },
      { re: /--?[A-Za-z][\w-]*/y, cls: 'tok-type' },
      { re: /\b\d[\d_.]*\b/y, cls: 'tok-number' },
      { re: /[A-Za-z_][\w.:\/@{}*-]*/y, cls: null }
    ],
    toml: [
      { re: /#[^\n]*/y, cls: 'tok-comment' },
      { re: /\[\[?[^\]\n]+\]\]?/y, cls: 'tok-keyword' },
      { re: /"(?:[^"\\\n]|\\.)*"/y, cls: 'tok-string' },
      { re: /\b(?:true|false)\b/y, cls: 'tok-keyword' },
      { re: /^[A-Za-z_][\w-]*(?=\s*=)/my, cls: 'tok-type' },
      { re: /\b\d[\d_]*\b/y, cls: 'tok-number' }
    ],
    yaml: [
      { re: /#[^\n]*/y, cls: 'tok-comment' },
      { re: /"(?:[^"\\\n]|\\.)*"/y, cls: 'tok-string' },
      { re: /'[^'\n]*'/y, cls: 'tok-string' },
      { re: /[A-Za-z_][\w-]*(?=:\s)/y, cls: 'tok-type' },
      { re: /\b(?:true|false|always)\b/y, cls: 'tok-keyword' }
    ],
    json: [
      { re: /"(?:[^"\\\n]|\\.)*"(?=\s*:)/y, cls: 'tok-type' },
      { re: /"(?:[^"\\\n]|\\.)*"/y, cls: 'tok-string' },
      { re: /\b(?:true|false|null)\b/y, cls: 'tok-keyword' },
      { re: /-?\b\d+(?:\.\d+)?\b/y, cls: 'tok-number' }
    ]
  };
  RULES.shell = RULES.bash;

  function escapeHtml(s) {
    return s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
  }

  function highlight(text, rules) {
    var out = '';
    var plain = '';
    var i = 0;
    while (i < text.length) {
      var matched = false;
      for (var r = 0; r < rules.length; r++) {
        var rule = rules[r];
        rule.re.lastIndex = i;
        var m = rule.re.exec(text);
        if (m && m[0].length > 0) {
          if (plain) { out += escapeHtml(plain); plain = ''; }
          out += rule.cls ? '<span class="' + rule.cls + '">' + escapeHtml(m[0]) + '</span>' : escapeHtml(m[0]);
          i += m[0].length;
          matched = true;
          break;
        }
      }
      if (!matched) { plain += text[i]; i++; }
    }
    if (plain) { out += escapeHtml(plain); }
    return out;
  }

  function languageOf(codeEl) {
    var m = (codeEl.className || '').match(/language-([\w-]+)/);
    return m ? m[1] : 'text';
  }

  var codes = document.querySelectorAll('.codeblock pre > code');
  for (var c = 0; c < codes.length; c++) {
    var rules = RULES[languageOf(codes[c])];
    if (!rules) { continue; }
    try {
      codes[c].innerHTML = highlight(codes[c].textContent, rules);
    } catch (err) {
      /* leave the plain text alone */
    }
  }

  /* ---------------- copy buttons ---------------- */

  var blocks = document.querySelectorAll('.codeblock');
  for (var b = 0; b < blocks.length; b++) {
    (function (block) {
      var head = block.querySelector('.cb-head');
      var pre = block.querySelector('pre');
      if (!head || !pre) { return; }
      var button = document.createElement('button');
      button.type = 'button';
      button.className = 'copy';
      button.textContent = 'Copy';
      button.setAttribute('aria-label', 'Copy code to clipboard');
      button.addEventListener('click', function () {
        var text = pre.textContent;
        var done = function () {
          button.textContent = 'Copied';
          window.setTimeout(function () { button.textContent = 'Copy'; }, 1400);
        };
        if (navigator.clipboard && navigator.clipboard.writeText) {
          navigator.clipboard.writeText(text).then(done, function () { button.textContent = 'Press Ctrl+C'; });
        } else {
          var ta = document.createElement('textarea');
          ta.value = text;
          document.body.appendChild(ta);
          ta.select();
          try { document.execCommand('copy'); done(); } catch (e) { button.textContent = 'Press Ctrl+C'; }
          document.body.removeChild(ta);
        }
      });
      head.appendChild(button);
    })(blocks[b]);
  }

  /* ---------------- screen tabs ---------------- */

  var groups = document.querySelectorAll('.screens');
  for (var g = 0; g < groups.length; g++) {
    (function (group) {
      var screens = group.querySelectorAll(':scope > .screen');
      if (screens.length < 2) { return; }
      group.classList.add('tabbed');
      var bar = document.createElement('div');
      bar.className = 'screen-tabs';
      bar.setAttribute('role', 'tablist');
      var buttons = [];
      function show(index) {
        for (var s = 0; s < screens.length; s++) {
          screens[s].classList.toggle('on', s === index);
          buttons[s].setAttribute('aria-selected', s === index ? 'true' : 'false');
        }
      }
      for (var s = 0; s < screens.length; s++) {
        (function (index) {
          var button = document.createElement('button');
          button.type = 'button';
          button.setAttribute('role', 'tab');
          button.textContent = screens[index].getAttribute('data-label') || ('Screen ' + (index + 1));
          button.addEventListener('click', function () { show(index); });
          buttons.push(button);
          bar.appendChild(button);
        })(s);
      }
      group.insertBefore(bar, group.firstChild);
      show(0);
    })(groups[g]);
  }

  /* ---------------- mobile nav ---------------- */

  var sidebar = document.querySelector('.sidebar');
  var toggle = document.querySelector('.nav-toggle');
  if (sidebar && toggle) {
    toggle.addEventListener('click', function () {
      var open = sidebar.classList.toggle('open');
      toggle.setAttribute('aria-expanded', open ? 'true' : 'false');
    });
    sidebar.addEventListener('click', function (e) {
      if (e.target.closest && e.target.closest('a')) {
        sidebar.classList.remove('open');
        toggle.setAttribute('aria-expanded', 'false');
      }
    });
  }

  /* ---------------- active section in the nav ---------------- */

  var links = {};
  var navAnchors = document.querySelectorAll('.sidebar a[href^="#"]');
  for (var n = 0; n < navAnchors.length; n++) {
    links[navAnchors[n].getAttribute('href').slice(1)] = navAnchors[n];
  }

  var targets = [];
  var all = document.querySelectorAll('.section[id], .sub[id]');
  for (var t = 0; t < all.length; t++) {
    if (links[all[t].id]) { targets.push(all[t]); }
  }

  var current = null;
  function setActive(id) {
    if (id === current) { return; }
    current = id;
    var parent = null;
    var el = document.getElementById(id);
    if (el && el.classList.contains('sub')) {
      var section = el.closest('.section');
      if (section) { parent = section.id; }
    }
    for (var key in links) {
      if (Object.prototype.hasOwnProperty.call(links, key)) {
        links[key].classList.toggle('active', key === id);
        links[key].classList.toggle('active-parent', key === parent);
      }
    }
    var link = links[id];
    if (link && sidebar && window.getComputedStyle(sidebar).position === 'sticky' && sidebar.scrollHeight > sidebar.clientHeight) {
      var lr = link.getBoundingClientRect();
      var sr = sidebar.getBoundingClientRect();
      if (lr.top < sr.top + 20 || lr.bottom > sr.bottom - 20) {
        sidebar.scrollTop += lr.top - sr.top - sr.height / 3;
      }
    }
  }

  function shown(el) { return el.getClientRects().length > 0; }

  /* The active entry is the last section or sub-section whose top has passed
     the line just under the top bar. Sub-sections come after their section in
     document order, so inside one the sub-section wins. Sections hidden by the
     Terminal / Dashboard switch are skipped. */
  function update() {
    var line = (parseFloat(window.getComputedStyle(document.documentElement).getPropertyValue('--topbar-h')) || 56) + 34;
    var visible = [];
    for (var v = 0; v < targets.length; v++) {
      if (shown(targets[v])) { visible.push(targets[v]); }
    }
    var best = visible.length ? visible[0].id : null;
    for (var i = 0; i < visible.length; i++) {
      if (visible[i].getBoundingClientRect().top <= line) { best = visible[i].id; }
      else { break; }
    }
    if ((window.innerHeight + window.scrollY) >= document.documentElement.scrollHeight - 4 && visible.length) {
      best = visible[visible.length - 1].id;
    }
    if (best) { setActive(best); }
  }

  var pending = false;
  window.addEventListener('scroll', function () {
    if (pending) { return; }
    pending = true;
    window.requestAnimationFrame(function () { pending = false; update(); });
  }, { passive: true });
  window.addEventListener('resize', update);

  /* ---------------- Terminal / Dashboard switch ---------------- */

  /* The head script has already set html[data-ui] from ?ui=, then localStorage,
     then "terminal", so the page is drawn in the right mode. Content marked
     data-only="terminal" or "dashboard" shows in that mode; the rest in both. */
  var root = document.documentElement;
  var STORE = 'redis-lens.docs.ui';
  var choices = document.querySelectorAll('.ui-switch [data-ui-choice]');

  function currentUi() { return root.getAttribute('data-ui') === 'dashboard' ? 'dashboard' : 'terminal'; }

  function modeOf(el) {
    var only = el && el.closest ? el.closest('[data-only]') : null;
    return only ? only.getAttribute('data-only') : null;
  }

  function reflect() {
    var ui = currentUi();
    for (var i = 0; i < choices.length; i++) {
      choices[i].setAttribute('aria-pressed', choices[i].getAttribute('data-ui-choice') === ui ? 'true' : 'false');
    }
  }

  function remember(ui) {
    try { window.localStorage.setItem(STORE, ui); } catch (e) { /* storage off: the URL still says it */ }
    try {
      var url = new URL(window.location.href);
      url.searchParams.set('ui', ui);
      window.history.replaceState(window.history.state, '', url.toString());
    } catch (e) { /* leave the address alone */ }
  }

  function setUi(ui) {
    root.setAttribute('data-ui', ui);
    reflect();
  }

  /* Switch by the buttons: keep the reader's place when the section they are in
     stays; otherwise go to the nearest section before it that does. */
  function choose(ui) {
    if (ui === currentUi()) { remember(ui); return; }
    var here = current ? document.getElementById(current) : null;
    var before = here && shown(here) ? here.getBoundingClientRect().top : null;
    setUi(ui);
    remember(ui);
    if (here && shown(here) && before !== null) {
      window.scrollBy(0, here.getBoundingClientRect().top - before);
    } else if (here) {
      var index = targets.indexOf(here);
      var back = null;
      for (var i = index - 1; i >= 0; i--) {
        if (shown(targets[i])) { back = targets[i]; break; }
      }
      if (back) { back.scrollIntoView(); } else { window.scrollTo(0, 0); }
    }
    current = null;
    update();
  }

  for (var ch = 0; ch < choices.length; ch++) {
    choices[ch].addEventListener('click', function (e) {
      choose(e.currentTarget.getAttribute('data-ui-choice'));
    });
  }
  reflect();

  /* A link to something that only shows in the other mode switches to it for this
     page view (without changing the remembered choice) and scrolls there. */
  function followHash() {
    var id = window.location.hash.slice(1);
    if (!id) { return false; }
    var el = document.getElementById(id);
    if (!el) { return false; }
    var mode = modeOf(el);
    if (mode && mode !== currentUi()) {
      setUi(mode);
      el.scrollIntoView();
    }
    return true;
  }
  window.addEventListener('hashchange', function () {
    followHash();
    current = null;
    update();
  });

  followHash();
  if (window.location.hash && links[window.location.hash.slice(1)]) {
    setActive(window.location.hash.slice(1));
  } else {
    update();
  }
})();
