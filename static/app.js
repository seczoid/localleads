// LocalLeads client glue: Alpine stores, live updates over SSE, and htmx hooks.
(function () {
  // Copies text, falling back to a hidden text area where the clipboard
  // API isn't allowed (plain http on a LAN address).
  function copyText(text, done) {
    const fail = () => toast('error', 'Copy failed; select the text instead');
    if (navigator.clipboard && window.isSecureContext) {
      navigator.clipboard.writeText(text).then(done, fail);
      return;
    }
    const area = document.createElement('textarea');
    area.value = text;
    area.setAttribute('readonly', '');
    area.style.position = 'fixed';
    area.style.opacity = '0';
    document.body.appendChild(area);
    area.select();
    try { document.execCommand('copy') ? done() : fail(); } catch (e) { fail(); }
    area.remove();
  }

  // A brief tick on a small copy button.
  function flashCopied(button) {
    button.classList.add('copied');
    setTimeout(() => button.classList.remove('copied'), 1200);
  }

  // One log line as text: "time  LEVEL  message".
  function logLine(li) {
    const part = (selector) => (li.querySelector(selector)?.textContent || '').trim();
    return `${part('time')}  ${part('.lvl').toUpperCase().padEnd(5)}  ${part('.msg')}`;
  }

  'use strict';

  let toastSeq = 0;
  const recent = new Map();

  function toast(level, message, undo) {
    if (!message || !window.Alpine) return;
    // The same notice can arrive both as a response header and over SSE.
    const key = level + '|' + message;
    const now = Date.now();
    if (recent.has(key) && now - recent.get(key) < 2500) {
      if (undo) Alpine.store('toasts').offerUndo(level, message, undo);
      return;
    }
    recent.set(key, now);
    Alpine.store('toasts').push(level, message, undo);
  }
  window.llToast = toast;

  // ---- system notifications when a search ends --------------------------
  function notifyState() {
    if (!('Notification' in window)) return 'unsupported';
    if (!window.isSecureContext) return 'insecure';
    return Notification.permission; // default | granted | denied
  }
  function notifyWanted() {
    try { return localStorage.getItem('ll-notify') !== 'off'; } catch (e) { return true; }
  }
  function showNotification(title, body, jobId) {
    try {
      const note = new Notification(title, {
        body,
        tag: jobId ? 'll-job-' + jobId : 'll-test',
        icon: '/static/favicon.svg',
      });
      note.onclick = () => {
        window.focus();
        if (jobId) window.location.href = '/jobs/' + encodeURIComponent(jobId);
        note.close();
      };
    } catch (e) { /* the browser refused */ }
  }
  function notifyFinished(job) {
    if (notifyState() !== 'granted' || !notifyWanted()) return;
    // Someone looking at the page already sees the toast.
    if (document.visibilityState === 'visible' && document.hasFocus()) return;
    const status = String(job.status || '').toLowerCase();
    const title = status === 'completed' ? 'Search finished'
      : status === 'failed' ? 'Search failed' : 'Search stopped';
    showNotification(title, job.name + (job.message ? ': ' + job.message : ''), job.id);
  }
  // Set by the update card just before it reloads into the new version.
  document.addEventListener('alpine:initialized', () => {
    let updated = null;
    try {
      updated = sessionStorage.getItem('ll-updated');
      sessionStorage.removeItem('ll-updated');
    } catch (e) { /* private mode */ }
    if (updated) toast('ok', `Updated to v${updated}. Running searches carried on.`);
  });

  document.addEventListener('alpine:init', () => {
    Alpine.store('toasts', {
      items: [],
      push(level, message, undo) {
        const id = ++toastSeq;
        this.items.push({ id, level: level || 'info', message: String(message).slice(0, 400), undo: undo || null, undoing: false });
        if (this.items.length > 5) this.items.shift();
        setTimeout(() => this.dismiss(id), undo ? 12000 : level === 'error' ? 9000 : 4500);
      },
      // A copy of a toast already showing arrived with an Undo link.
      offerUndo(level, message, undo) {
        const shown = this.items.find((t) => t.level === (level || 'info') && t.message === String(message).slice(0, 400));
        if (shown && !shown.undo) shown.undo = undo;
      },
      undo(item) {
        if (!item.undo || item.undoing) return;
        item.undoing = true;
        htmx.ajax('POST', item.undo, { source: document.body, swap: 'none' })
          .finally(() => this.dismiss(item.id));
      },
      dismiss(id) {
        this.items = this.items.filter((t) => t.id !== id);
      },
    });

    Alpine.store('conn', { online: false });

    const isDark = () => {
      const root = document.documentElement;
      return root.dataset.theme
        ? root.dataset.theme === 'dark'
        : window.matchMedia('(prefers-color-scheme: dark)').matches;
    };
    Alpine.store('theme', {
      dark: isDark(),
      toggle() {
        const root = document.documentElement;
        root.dataset.theme = isDark() ? 'light' : 'dark';
        this.dark = root.dataset.theme === 'dark';
        try { localStorage.setItem('ll-theme', root.dataset.theme); } catch (e) { /* private mode */ }
      },
    });

    // Which lead-filter dropdown is open, and what's typed in its search box.
    // Lives in a store so it survives the filter bar being re-rendered after
    // every change.
    Alpine.store('facets', {
      open: '',
      q: {},
      toggle(name) { this.open = this.open === name ? '' : name; },
    });
    // Escape closes an open filter dropdown first, before anything else
    // (such as the lead side panel) reacts to it.
    window.addEventListener('keydown', (event) => {
      const facets = Alpine.store('facets');
      if (event.key !== 'Escape' || !facets.open) return;
      facets.open = '';
      event.stopImmediatePropagation();
    }, true);

    // New-job form: location mode, limits, and the filter builder.
    Alpine.data('jobForm', (filters, collectAll) => ({
      filters: Array.isArray(filters) ? filters : [],
      collectAll: !!collectAll,
      error: '',
      addFilter() {
        this.filters.push({ field: 'rating', op: '>=', value: '4' });
      },
      serialize(f) {
        if (!f || !f.field || !f.op) return '';
        if (f.op === 'is_empty' || f.op === 'not_empty') return f.field + ' ' + f.op;
        return f.field + ' ' + f.op + ' ' + (f.value || '').trim();
      },
    }));

    // Business type picker: Google Maps categories with autocomplete, plus
    // anything typed. Keeps the hidden `queries` textarea (one per line) in
    // step, so the preview and the form post work as before.
    const foldText = (s) => String(s).normalize('NFD').replace(/[\u0300-\u036f]/g, '').replace(/[\u2019\u2018`]/g, "'").toLowerCase();
    const escapeHtml = (s) => String(s).replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' })[c]);
    const capitalize = (s) => s.charAt(0).toUpperCase() + s.slice(1);
    // Identity of a query: case-insensitive, but "cafe" and "café" are two.
    const queryKey = (s) => String(s).replace(/[\u2019\u2018`]/g, "'").replace(/\s+/g, ' ').trim().toLowerCase();
    let categoryData = null;
    const loadCategories = (url) => (categoryData ||= fetch(url, { credentials: 'same-origin' })
      .then((r) => (r.ok ? r.json() : Promise.reject(new Error(r.statusText))))
      .catch((err) => { categoryData = null; throw err; }));
    const CHIP_LIMIT = 14;
    const ROW_LIMIT = 120;
    Alpine.data('categoryPicker', (url) => ({
      ready: false,
      failed: false,
      groups: [],
      total: 0,
      picked: [],
      q: '',
      open: false,
      tab: 'categories',
      group: null,
      active: 0,
      expanded: false,
      rev: 0,
      init() {
        // Not reactive: thousands of entries Alpine doesn't need to watch.
        this._names = new Map();   // queryKey(name) -> { name, fold, group, rank }
        this._all = [];
        this._memo = null;
        const seen = new Set();
        this.picked = this.$refs.value.value.split(/[\n|]+/).map((s) => s.trim()).filter((s) => {
          const key = queryKey(s);
          if (!s || seen.has(key)) return false;
          seen.add(key);
          return true;
        });
        loadCategories(url).then((data) => {
          const groups = (data && data.groups) || [];
          for (const g of groups) {
            g.items.forEach((name, rank) => {
              const entry = { name, key: g.key + ':' + rank, fold: foldText(name), group: g, rank };
              if (!this._names.has(queryKey(name))) this._names.set(queryKey(name), entry);
              this._all.push(entry);
            });
          }
          this.groups = groups.map((g) => ({ key: g.key, label: g.label, items: g.items }));
          this._groupByKey = new Map(this.groups.map((g) => [g.key, g]));
          this.total = this._all.length;
          // Picked entries take Google's spelling once the list is in.
          this.picked = this.picked.map((q) => this._names.get(queryKey(q))?.name || q);
          this.ready = true;
          this.touch(false);
        }).catch(() => { this.failed = true; });
      },

      // ---- state
      touch(notify = true) {
        this._picked = new Set(this.picked.map(queryKey));
        this._memo = null;
        this.rev++;
        const box = this.$refs.value;
        const value = this.picked.join('\n');
        if (box.value !== value) {
          box.value = value;
          if (notify) box.dispatchEvent(new Event('change', { bubbles: true }));
        }
      },
      isPicked(name) { return (this._picked || new Set()).has(queryKey(name)); },
      known(q) { return this.ready && this._names.has(queryKey(q)); },
      pretty(q) { return this.known(q) ? capitalize(this._names.get(queryKey(q)).name) : q; },
      add(name) {
        const text = String(name).replace(/\s+/g, ' ').trim();
        if (!text || this.isPicked(text)) return;
        this.picked.push(this._names.get(queryKey(text))?.name || text);
        this.touch();
      },
      addMany(names) {
        let changed = false;
        for (const name of names) {
          const text = String(name).replace(/\s+/g, ' ').trim();
          if (!text || this._picked?.has(queryKey(text))) continue;
          this.picked.push(this._names.get(queryKey(text))?.name || text);
          this._picked = new Set(this.picked.map(queryKey));
          changed = true;
        }
        if (changed) this.touch();
      },
      remove(name) {
        const key = queryKey(name);
        this.picked = this.picked.filter((p) => queryKey(p) !== key);
        this.touch();
      },
      toggle(name) { this.isPicked(name) ? this.remove(name) : this.add(name); },
      clearAll() { this.picked = []; this.expanded = false; this.touch(); this.focusInput(); },
      groupPicked(g) {
        void this.rev;
        let n = 0;
        for (const name of g.items) if (this._picked?.has(queryKey(name))) n++;
        return n;
      },
      toggleGroup(g) {
        if (this.groupPicked(g) === g.items.length) {
          const drop = new Set(g.items.map(queryKey));
          this.picked = this.picked.filter((p) => !drop.has(queryKey(p)));
          this.touch();
        } else {
          this.addMany(g.items);
        }
      },
      shownChips() {
        return this.expanded || this.picked.length <= CHIP_LIMIT ? this.picked : this.picked.slice(0, CHIP_LIMIT);
      },
      hiddenCount() { return this.expanded ? 0 : Math.max(0, this.picked.length - CHIP_LIMIT); },

      // ---- the list
      custom() {
        const text = this.q.trim();
        return this.tab === 'categories' && !!text && !this.known(text) && !this.isPicked(text);
      },
      mark(name, at, length) {
        if (at < 0 || !length) return escapeHtml(capitalize(name));
        const shown = capitalize(name);
        return escapeHtml(shown.slice(0, at)) + '<mark>' + escapeHtml(shown.slice(at, at + length)) + '</mark>' + escapeHtml(shown.slice(at + length));
      },
      row(entry, at, length, meta) {
        return { key: entry.key, name: entry.name, html: this.mark(entry.name, at, length), meta, on: this.isPicked(entry.name), group: null };
      },
      rows() {
        void this.rev;
        if (!this.ready) return [];
        const query = foldText(this.q.trim());
        const memoKey = this.tab + '|' + (this.group ? this.group.key : '') + '|' + query + '|' + this.rev;
        if (this._memo && this._memo.key === memoKey) return this._memo.rows;
        let rows;
        if (this.tab === 'groups' && !this.group) {
          rows = this.groups
            .filter((g) => !query || foldText(g.label).includes(query))
            .map((g) => {
              const on = this.groupPicked(g);
              const at = query ? foldText(g.label).indexOf(query) : -1;
              return {
                key: 'g:' + g.key, group: g, on: false,
                html: at < 0 ? escapeHtml(g.label) : escapeHtml(g.label.slice(0, at)) + '<mark>' + escapeHtml(g.label.slice(at, at + query.length)) + '</mark>' + escapeHtml(g.label.slice(at + query.length)),
                meta: (on ? on.toLocaleString() + ' of ' : '') + g.items.length.toLocaleString(),
              };
            });
        } else if (this.tab === 'groups') {
          const g = this._groupByKey.get(this.group.key);
          rows = [];
          for (const entry of this._all) {
            if (entry.group.key !== g.key) continue;
            const at = query ? entry.fold.indexOf(query) : -1;
            if (query && at < 0) continue;
            rows.push(this.row(entry, at, query.length, ''));
          }
        } else if (!query) {
          // Nothing typed: the most common few of every group.
          const top = this._all.filter((e) => e.rank < 5).sort((a, b) => a.rank - b.rank || a.group.label.localeCompare(b.group.label));
          rows = top.map((e) => this.row(e, -1, 0, e.group.label));
        } else {
          const scored = [];
          for (const entry of this._all) {
            const at = entry.fold.indexOf(query);
            if (at < 0) continue;
            let score;
            if (entry.fold === query) score = 0;
            else if (at === 0) score = 1;
            else if (/[\s\-\/(&']/.test(entry.fold[at - 1])) score = 2;
            else score = 3;
            scored.push([score, entry.rank, entry, at]);
          }
          scored.sort((a, b) => a[0] - b[0] || a[1] - b[1] || a[2].fold.length - b[2].fold.length);
          rows = scored.slice(0, ROW_LIMIT).map(([, , e, at]) => this.row(e, at, query.length, e.group.label));
        }
        this._memo = { key: memoKey, rows };
        return rows;
      },
      activeId() {
        if (this.active === -1) return this.custom() ? 'cat-opt-own' : null;
        return this.rows()[this.active] ? 'cat-opt-' + this.active : null;
      },

      // ---- interaction
      focusInput() { this.$refs.input.focus(); },
      show() {
        if (this.open) return;
        this.open = true;
        this.active = this.rows().length ? 0 : -1;
      },
      close() { this.open = false; },
      leave(event) {
        if (!this.$el.contains(event.relatedTarget)) this.close();
      },
      setTab(tab) {
        this.tab = tab;
        this.group = null;
        this.active = this.rows().length ? 0 : (this.custom() ? -1 : 0);
        this.$refs.list.scrollTop = 0;
        this.focusInput();
      },
      typed() {
        this.open = true;
        this.active = this.rows().length ? 0 : -1;
        this.$nextTick(() => { if (this.$refs.list) this.$refs.list.scrollTop = 0; });
      },
      move(step) {
        if (!this.open) { this.show(); return; }
        const count = this.rows().length;
        const min = this.custom() ? -1 : 0;
        if (!count && min === 0) return;
        let next = this.active + step;
        if (next < min) next = count - 1;
        if (next > count - 1) next = min;
        this.active = next;
        this.$nextTick(() => document.getElementById(this.activeId() || '')?.scrollIntoView({ block: 'nearest' }));
      },
      choose(row, fromKey = false) {
        if (row.group) {
          this.group = row.group;
          this.q = '';
          this.active = 0;
          this.$refs.list.scrollTop = 0;
          this.focusInput();
          return;
        }
        if (fromKey) {
          // Enter adds (never removes) and starts the next search.
          this.add(row.name);
          if (this.tab === 'categories') { this.q = ''; this.active = 0; }
        } else {
          this.toggle(row.name);
        }
        this.focusInput();
      },
      enter() {
        if (!this.open) { this.show(); return; }
        const text = this.q.trim();
        if (this.active === -1) {
          if (this.custom()) { this.add(text); this.q = ''; this.active = 0; }
          return;
        }
        const row = this.rows()[this.active];
        if (row) this.choose(row, true);
        else if (text) { this.add(text); this.q = ''; }
      },
      comma(event) {
        if (event.key !== ',' || !this.q.trim()) return;
        event.preventDefault();
        this.add(this.q);
        this.q = '';
        this.active = 0;
      },
      backspace(event) {
        if (this.q !== '' || !this.picked.length) return;
        event.preventDefault();
        this.picked.pop();
        this.touch();
      },
      tabAway() { this.close(); },
      paste(event) {
        const text = (event.clipboardData || window.clipboardData)?.getData('text') || '';
        const parts = text.split(/[\r\n|\t]+/).map((s) => s.trim()).filter(Boolean);
        if (parts.length < 2) return;
        event.preventDefault();
        this.addMany(parts);
        this.q = '';
      },
    }));

    // A menu that floats above the page (position: fixed) so scrolling
    // tables can't clip it; it opens upward when there's no room below.
    Alpine.data('dropdown', () => ({
      open: false,
      toggle() {
        this.open = !this.open;
        if (this.open) this.$nextTick(() => this.place());
      },
      place() {
        const button = this.$refs.button;
        const menu = this.$refs.menu;
        if (!button || !menu) return;
        const rect = button.getBoundingClientRect();
        const height = menu.offsetHeight;
        const width = menu.offsetWidth;
        const roomBelow = window.innerHeight - rect.bottom;
        const top = roomBelow < height + 12 && rect.top > height + 12 ? rect.top - height - 6 : rect.bottom + 6;
        menu.style.top = Math.max(8, top) + 'px';
        menu.style.left = Math.max(8, Math.min(rect.right - width, window.innerWidth - width - 8)) + 'px';
      },
    }));

    // Location picker on the new-search form: a country, then whole states
    // or single cities (US). The server turns each into its ZIP codes.
    const compactNumber = (n) => (n >= 1e6 ? (n / 1e6).toFixed(n >= 1e7 ? 0 : 1) + 'M' : n >= 1e3 ? Math.round(n / 1e3) + 'k' : String(n));
    Alpine.data('regionPicker', (initial, custom) => ({
      country: initial.country,
      countries: initial.countries || [],
      expandable: initial.expandable,
      postal: initial.postal,
      postalNoun: initial.postal_noun || 'ZIP code',
      counts: initial.counts || {},
      areas: initial.areas || {},
      // Picked cities' postal-code counts, from the lists they came from.
      cityZips: {},
      depth: initial.depth || 'zip',
      depthChosen: !!initial.depth && initial.depth !== 'zip',
      codes: initial.codes || {},
      states: [],
      custom: !!custom,
      q: '',
      results: [],
      init() {
        this.setStates(initial.states, initial.all, initial.picked_states, initial.picked_cities);
        this.fitDepth();
      },
      us() {
        return this.country === 'United States';
      },
      // What a country's first-level parts are called here.
      unit(one) {
        if (this.us()) return one ? 'state' : 'states';
        return one ? 'region' : 'regions';
      },
      setStates(names, all, pickedStates, pickedCities) {
        const full = new Set(pickedStates || []);
        const byKey = {};
        this.states = (names || []).map((name) => {
          const state = { name, full: !!all || full.has(name), open: false, loading: false, cities: null, picked: new Set() };
          byKey[name.toLowerCase()] = state;
          if (this.codes[name]) byKey[this.codes[name].toLowerCase()] = state;
          return state;
        });
        // "Louisville, KY" or "Lyon, Auvergne-Rhône-Alpes": the last part names the state.
        (pickedCities || []).forEach((label) => {
          const state = byKey[label.split(',').pop().trim().toLowerCase()];
          if (state && !state.full) state.picked.add(label);
        });
      },

      // Country search: a type-to-filter list of every country.
      countryOpen: false,
      countryQ: '',
      countryActive: 0,
      isoOf(name) {
        const found = this.countries.find((c) => c[0] === name);
        return found ? found[1] : '';
      },
      fold(text) {
        return text.normalize('NFD').replace(/[\u0300-\u036f]/g, '').toLowerCase();
      },
      countryMatches() {
        const q = this.fold(this.countryQ.trim());
        if (!q) return this.countries;
        const starts = [];
        const contains = [];
        this.countries.forEach((c) => {
          const name = this.fold(c[0]);
          if (name.startsWith(q) || c[1].toLowerCase() === q) starts.push(c);
          else if (name.includes(q)) contains.push(c);
        });
        return starts.concat(contains);
      },
      openCountries() {
        if (this.countryOpen) return this.closeCountries();
        this.countryOpen = true;
        this.countryQ = '';
        this.countryActive = Math.max(0, this.countries.findIndex((c) => c[0] === this.country));
        this.$nextTick(() => {
          this.$refs.countrySearch.focus();
          this.scrollCountry();
        });
      },
      closeCountries() {
        if (!this.countryOpen) return;
        this.countryOpen = false;
        this.$refs.countryBtn.focus();
      },
      moveCountry(step) {
        const count = this.countryMatches().length;
        if (!count) return;
        this.countryActive = (this.countryActive + step + count) % count;
        this.scrollCountry();
      },
      scrollCountry() {
        this.$nextTick(() => {
          const row = document.getElementById('country-' + this.countryActive);
          if (row) row.scrollIntoView({ block: 'nearest' });
        });
      },
      pickActiveCountry() {
        const match = this.countryMatches()[this.countryActive];
        if (match) this.pickCountry(match[0]);
      },
      pickCountry(name) {
        this.closeCountries();
        if (name === this.country) return;
        this.country = name;
        this.loadCountry();
      },

      async loadCountry() {
        this.q = '';
        this.results = [];
        try {
          const response = await fetch('/locations/regions?country=' + encodeURIComponent(this.country));
          const data = await response.json();
          this.expandable = data.expandable;
          this.postal = data.postal;
          this.postalNoun = data.postal_noun || 'postal code';
          this.counts = data.counts || {};
          this.areas = data.areas || {};
          this.cityZips = {};
          this.codes = data.codes || {};
          this.setStates(data.states, true, [], []);
          this.fitDepth();
        } catch (e) {
          window.llToast && window.llToast('error', 'Could not load places for ' + this.country);
        }
        this.changed();
      },

      // How finely each picked area is split.
      depthOptions() {
        const all = this.allPicked();
        const count = (n) => (all && n ? n.toLocaleString() : '');
        const options = [];
        if (this.postal) options.push({ value: 'zip', label: this.us() ? 'ZIP codes' : 'Postal codes', count: count(this.counts.zip) });
        options.push({ value: 'city', label: this.us() ? 'Cities' : 'Towns', count: count(this.counts.city) });
        options.push({ value: 'region', label: 'Whole ' + this.unit(), count: count(this.counts.region) });
        return options;
      },
      // Without postal codes the finest split is by town; keep the finest
      // split unless one was chosen.
      fitDepth() {
        if (!this.postal && this.depth === 'zip') this.depth = 'city';
        else if (!this.depthChosen) this.depth = this.postal ? 'zip' : 'city';
      },
      setDepth(value) {
        this.depth = value;
        this.depthChosen = true;
        this.changed();
      },
      depthNote() {
        if (this.depth === 'zip') return 'Most leads, most lookups';
        if (this.depth === 'city') return this.postal ? 'Fewer lookups, misses some outskirts' : 'Every town of 1,000+ people';
        return 'Quickest, finds the fewest';
      },

      changed() {
        this.$nextTick(() => this.$root.dispatchEvent(new Event('change', { bubbles: true })));
      },
      cityQuery() {
        return '/locations/cities?country=' + encodeURIComponent(this.country);
      },
      async loadCities(state) {
        if (state.cities || !this.expandable) return;
        state.loading = true;
        try {
          const response = await fetch(this.cityQuery() + '&state=' + encodeURIComponent(state.name));
          state.cities = await response.json();
          state.cities.forEach((c) => { this.cityZips[c.label] = c.zips; });
        } finally {
          state.loading = false;
        }
      },
      async expand(state) {
        state.open = !state.open;
        if (state.open) await this.loadCities(state);
      },
      toggleState(state) {
        const on = !(state.full || state.picked.size);
        state.full = on;
        state.picked = new Set();
        this.changed();
      },
      cityPicked(state, city) {
        return !!state && (state.full || state.picked.has(city.label));
      },
      async toggleCity(state, city) {
        if (!state) return;
        await this.loadCities(state);
        const all = (state.cities || []).map((c) => c.label);
        if (state.full) {
          state.full = false;
          state.picked = new Set(all);
          state.picked.delete(city.label);
        } else if (state.picked.has(city.label)) {
          state.picked.delete(city.label);
        } else {
          state.picked.add(city.label);
        }
        if (all.length && state.picked.size === all.length) {
          state.full = true;
          state.picked = new Set();
        }
        this.changed();
      },
      stateOf(city) {
        return this.states.find((s) => s.name === city.state);
      },
      async search() {
        const q = this.q.trim();
        if (!this.expandable || q.length < 2) {
          this.results = [];
          return;
        }
        const country = this.country;
        try {
          const response = await fetch(this.cityQuery() + '&q=' + encodeURIComponent(q));
          const found = await response.json();
          if (q === this.q.trim() && country === this.country) {
            this.results = found;
            found.forEach((c) => { this.cityZips[c.label] = c.zips; });
          }
        } catch (e) {
          this.results = [];
        }
      },
      visibleStates() {
        const q = this.fold(this.q.trim());
        return q ? this.states.filter((s) => this.fold(s.name).includes(q)) : this.states;
      },
      allPicked() {
        return this.states.every((s) => s.full);
      },
      fullStates() {
        return this.states.filter((s) => s.full).map((s) => s.name);
      },
      pickedCities() {
        return this.states.flatMap((s) => (s.full ? [] : Array.from(s.picked)));
      },
      selectAll() {
        this.states.forEach((s) => { s.full = true; s.picked = new Set(); });
        this.changed();
      },
      clearAll() {
        this.states.forEach((s) => { s.full = false; s.picked = new Set(); });
        this.changed();
      },
      // Chip objects carry a `removable` flag for x-show: Alpine calls a
      // function an expression evaluates to, so `remove` itself can't be used.
      chips() {
        return this.chipList().map((chip) => ({ ...chip, removable: !!chip.remove }));
      },
      chipList() {
        if (!this.states.length) return [{ key: 'country', label: this.country, remove: null }];
        if (this.allPicked()) {
          return [{ key: 'all', label: `All of ${this.countryName()} (${this.states.length} ${this.unit(this.states.length === 1)})`, remove: () => this.clearAll() }];
        }
        const chips = [];
        this.states.forEach((s) => {
          if (s.full) chips.push({ key: 's:' + s.name, label: s.name, remove: () => { s.full = false; this.changed(); } });
          s.picked.forEach((label) => chips.push({ key: 'c:' + label, label, remove: () => { s.picked.delete(label); this.changed(); } }));
        });
        if (chips.length > 12) {
          const more = chips.length - 10;
          return chips.slice(0, 10).concat([{ key: 'more', label: `+${more} more`, remove: null }]);
        }
        return chips;
      },
      summary() {
        if (!this.states.length) return '';
        if (this.allPicked()) return `All ${this.states.length} selected`;
        const states = this.fullStates().length;
        const cities = this.pickedCities().length;
        const parts = [];
        if (states) parts.push(states + ' ' + this.unit(states === 1));
        if (cities) parts.push(cities + (cities === 1 ? ' city' : ' cities'));
        return parts.length ? parts.join(', ') + ' selected' : 'Nothing selected';
      },
      // What one area is called at the current split.
      areaNoun(count) {
        const noun = this.depth === 'zip' ? this.postalNoun : this.us() ? 'city' : 'town';
        if (count === 1) return noun;
        return noun === 'city' ? 'cities' : noun + 's';
      },
      // How many lookups a whole state makes at the current split.
      stateAreas(state) {
        const counts = this.areas[state.name];
        if (!counts || this.depth === 'region') return 0;
        return this.depth === 'zip' && this.postal ? counts[0] : counts[1];
      },
      stateText(state) {
        const n = this.stateAreas(state);
        return n ? `${n.toLocaleString()} ${this.areaNoun(n)}` : '';
      },
      // Lookups the picked places make, or null when a picked city's count
      // isn't known yet.
      pickedAreas() {
        if (this.depth === 'region') return null;
        let total = 0;
        for (const s of this.states) {
          if (s.full) total += this.stateAreas(s);
          for (const label of s.full ? [] : s.picked) {
            if (this.depth !== 'zip' || !this.postal) total += 1;
            else if (label in this.cityZips) total += Math.max(1, this.cityZips[label]);
            else return null;
          }
        }
        return total;
      },
      zipText(city) {
        if (city.zips > 1) return `${city.zips.toLocaleString()} ${this.postalNoun}s`;
        if (city.population) return `${compactNumber(city.population)} people`;
        return '';
      },
      // The closed picker reads as a sentence: "All of the United States",
      // "Texas, Ohio and 3 more".
      editing: false,
      countryName() {
        return /^(United |Netherlands$|Philippines$|Czech|Dominican|Bahamas$|Gambia$|Maldives$|Central African|Democratic|Marshall|Solomon|Cayman|Comoros$|Seychelles$)/.test(this.country) ? 'the ' + this.country : this.country;
      },
      choiceText() {
        if (!this.states.length) return 'All of ' + this.countryName();
        if (this.allPicked()) return 'All of ' + this.countryName();
        const names = this.fullStates().concat(this.pickedCities());
        if (!names.length) return 'No places picked yet';
        if (names.length <= 3) return names.join(', ');
        return names.slice(0, 2).join(', ') + ` and ${names.length - 2} more`;
      },
      choiceDetail() {
        if (!this.states.length) return '';
        const names = this.fullStates().length + this.pickedCities().length;
        if (!this.allPicked()) {
          if (!names) return 'Pick at least one place';
          const total = this.pickedAreas();
          return total ? `${this.summary()}: ${total.toLocaleString()} ${this.areaNoun(total)} to search` : this.summary();
        }
        const regions = `${this.states.length.toLocaleString()} ${this.unit(this.states.length === 1)}`;
        if (!this.expandable) return regions;
        const by = { zip: `${this.postalNoun} by ${this.postalNoun}`, city: this.us() ? 'city by city' : 'town by town', region: `one lookup per ${this.unit(true)}` }[this.depth];
        return `${regions}, searched ${by}`;
      },
    }));

    // Table selection (proxies, leads), kept across refreshes and pages.
    const selection = () => ({
      selected: new Set(),
      pageIds: [],
      matching: 0,
      allMatching: false,
      ids() { return Array.from(this.selected); },
      count() { return this.allMatching ? this.matching : this.selected.size; },
      isSelected(id) { return this.allMatching || this.selected.has(id); },
      toggle(id) {
        if (this.allMatching) {
          this.allMatching = false;
          this.pageIds.forEach((pid) => this.selected.add(pid));
        }
        if (this.selected.has(id)) this.selected.delete(id); else this.selected.add(id);
      },
      pageSelected() {
        return this.pageIds.length > 0 && this.pageIds.every((id) => this.isSelected(id));
      },
      togglePage(on) {
        this.allMatching = false;
        this.pageIds.forEach((id) => (on ? this.selected.add(id) : this.selected.delete(id)));
      },
      clear() {
        this.selected.clear();
        this.allMatching = false;
      },
      // The lead filter form as a query string, for bulk actions on "all
      // matching".
      filterQs() {
        const form = document.getElementById('lead-filters');
        return form ? new URLSearchParams(new FormData(form)).toString() : '';
      },
      tableSort() {
        const table = document.getElementById('leads-dt');
        return (table && table.dataset.sort) || '';
      },
    });
    // The ProxyScrape strip on the Proxies page: hidden for 30 days once
    // dismissed, in this browser only.
    Alpine.data('offerStrip', (key) => ({
      shown: true,
      init() {
        try {
          const until = Number(localStorage.getItem(key) || 0);
          this.shown = !(until > Date.now());
        } catch (e) { this.shown = true; }
      },
      hide() {
        this.shown = false;
        try { localStorage.setItem(key, String(Date.now() + 30 * 86400000)); } catch (e) { /* private window */ }
      },
    }));
    // The update card while LocalLeads installs a new version and restarts
    // into it: waits for the new version to answer, then reloads.
    Alpine.data('updateRestart', (from) => ({
      slow: false,
      timer: null,
      init() {
        const started = Date.now();
        const poll = async () => {
          try {
            const response = await fetch('/update/status', { cache: 'no-store' });
            if (response.ok) {
              const status = await response.json();
              if (status.version !== from) {
                try { sessionStorage.setItem('ll-updated', status.version); } catch (e) { /* private mode */ }
                location.reload();
                return;
              }
              // Same version and no longer working on it: the update
              // didn't go through; the reloaded card says why.
              if (!status.restarting && !status.busy) { location.reload(); return; }
            }
          } catch (e) { /* still restarting */ }
          if (Date.now() - started > 120000) this.slow = true;
          this.timer = setTimeout(poll, 1500);
        };
        this.timer = setTimeout(poll, 1500);
      },
      destroy() { clearTimeout(this.timer); },
    }));
    // A search's log: find and level filters over the lines on screen,
    // copy, download, and reading back past the newest 200 lines (the live
    // list keeps refreshing; older lines show in a separate, still list).
    Alpine.data('jobLog', (id) => ({
      q: '',
      level: '',
      history: false,
      loading: false,
      loaded: 0,
      total: 0,
      matches: 0,
      lines: 0,
      onSwap: null,
      get filtered() { return this.q.trim() !== '' || this.level !== ''; },
      init() {
        this.$watch('q', () => this.apply());
        this.$watch('level', () => this.apply());
        this.onSwap = (event) => {
          const target = event.detail && event.detail.target;
          if (target && target.id === 'job-events') this.$nextTick(() => this.apply());
        };
        document.body.addEventListener('htmx:oobAfterSwap', this.onSwap);
        this.$nextTick(() => this.apply());
      },
      destroy() { document.body.removeEventListener('htmx:oobAfterSwap', this.onSwap); },
      list() { return this.$root.querySelector(this.history ? '#job-events-history' : '#job-events'); },
      apply() {
        const q = this.q.trim().toLowerCase();
        let matches = 0;
        let lines = 0;
        for (const list of this.$root.querySelectorAll('ol.log')) {
          const active = list === this.list();
          for (const li of list.querySelectorAll('li[data-level]')) {
            const ok = (!this.level || li.dataset.level === this.level)
              && (!q || li.textContent.toLowerCase().includes(q));
            li.hidden = !ok;
            if (active) { lines += 1; if (ok) matches += 1; }
          }
        }
        this.matches = matches;
        this.lines = lines;
      },
      summary() {
        if (!this.lines) return '';
        const n = (value) => value.toLocaleString();
        const shown = this.filtered ? `${n(this.matches)} of ` : '';
        if (this.history) return `${shown}${n(this.lines)} lines, of ${n(this.total)} in all`;
        const word = this.lines === 1 ? 'line' : 'lines';
        return this.lines >= 200 ? `${shown}Newest ${n(this.lines)} lines` : `${shown}${n(this.lines)} ${word}`;
      },
      canShowOlder() {
        return this.history ? this.loaded < this.total : this.lines >= 200;
      },
      async older() {
        this.loading = true;
        const limit = Math.max(this.history ? this.loaded : this.lines, 200) + 200;
        try {
          const response = await fetch(`/jobs/${id}/events?limit=${limit}`, { cache: 'no-store' });
          if (!response.ok) throw new Error(await response.text());
          this.$root.querySelector('#job-events-history').innerHTML = await response.text();
          this.total = Number(response.headers.get('x-event-total')) || 0;
          this.loaded = Math.min(limit, this.total);
          this.history = true;
          this.$nextTick(() => this.apply());
        } catch (error) {
          toast('error', `Couldn't load older lines: ${error.message || error}`);
        } finally {
          this.loading = false;
        }
      },
      backToLive() {
        this.history = false;
        this.$root.querySelector('#job-events-history').innerHTML = '';
        this.loaded = 0;
        this.$nextTick(() => this.apply());
      },
      copyAll() {
        const lines = [...this.list().querySelectorAll('li[data-level]:not([hidden])')].map(logLine);
        // The live list is newest first; copied text reads oldest first.
        lines.reverse();
        copyText(lines.join('\n'), () => toast('ok', `Copied ${lines.length.toLocaleString()} ${lines.length === 1 ? 'line' : 'lines'}`));
      },
      downloadUrl() {
        const params = new URLSearchParams();
        if (this.level) params.set('level', this.level);
        if (this.q.trim()) params.set('q', this.q.trim());
        const query = params.toString();
        return `/jobs/${id}/log.txt${query ? `?${query}` : ''}`;
      },
    }));
    // The CSV column chooser: presets, find, and the last choice kept in
    // this browser.
    // System page: the server's log, polled while following.
    Alpine.data('serverLog', () => ({
      lines: [],
      after: 0,
      dropped: 0,
      loaded: false,
      follow: true,
      q: '',
      level: '',
      timer: null,
      get filtered() { return this.q.trim() !== '' || this.level !== ''; },
      get shown() {
        const q = this.q.trim().toLowerCase();
        return this.lines.filter((line) => (!this.level || line.level === this.level)
          && (!q || line.message.toLowerCase().includes(q) || line.time.includes(q)));
      },
      init() {
        this.load();
        this.timer = setInterval(() => {
          if (this.follow && document.visibilityState === 'visible') this.load();
        }, 3000);
      },
      destroy() { clearInterval(this.timer); },
      async load() {
        try {
          const response = await fetch(`/system/log.json?after=${this.after}`, { cache: 'no-store' });
          if (!response.ok) return;
          const data = await response.json();
          this.dropped = data.dropped || 0;
          if (data.lines.length) {
            // Newest first, capped like the server's buffer.
            this.lines = data.lines.slice().reverse().concat(this.lines).slice(0, 3000);
            this.after = data.lines[data.lines.length - 1].seq;
          }
        } catch (e) { /* offline: try again on the next tick */ }
        this.loaded = true;
      },
      summary() {
        if (!this.lines.length) return '';
        const n = (value) => value.toLocaleString();
        const shown = this.filtered ? `${n(this.shown.length)} of ` : '';
        const older = this.dropped ? ` (${n(this.dropped)} older lines are only in the terminal or log file)` : '';
        return `${shown}${n(this.lines.length)} ${this.lines.length === 1 ? 'line' : 'lines'}${older}`;
      },
      copyAll() {
        const text = this.shown.slice().reverse()
          .map((line) => `${line.time}  ${line.level.toUpperCase().padEnd(5)}  ${line.message}`);
        copyText(text.join('\n'), () => toast('ok', `Copied ${text.length.toLocaleString()} ${text.length === 1 ? 'line' : 'lines'}`));
      },
      downloadUrl() {
        const params = new URLSearchParams();
        if (this.level) params.set('level', this.level);
        if (this.q.trim()) params.set('q', this.q.trim());
        const query = params.toString();
        return '/system/log.txt' + (query ? '?' + query : '');
      },
    }));

    // Settings: system notifications when a search ends.
    Alpine.data('finishAlerts', () => ({
      state: notifyState(),
      on: notifyWanted(),
      async allow() {
        try { await Notification.requestPermission(); } catch (e) { /* old Safari */ }
        this.state = notifyState();
        if (this.state === 'granted') this.set(true);
      },
      set(on) {
        this.on = on;
        try { localStorage.setItem('ll-notify', on ? 'on' : 'off'); } catch (e) { /* private mode */ }
      },
      test() {
        showNotification('Search finished', 'Plumbers in Austin, TX: 412 leads, 236 with email', null);
      },
    }));

    // CSV download: the clean layout (readable columns, one row per
    // business) or every Outscraper-style column. Choices are remembered.
    const readSaved = (key) => {
      try { return JSON.parse(localStorage.getItem(key) || 'null'); } catch (e) { return null; }
    };
    Alpine.data('csvColumns', () => ({
      style: 'clean',
      chosen: [],
      clean: [],
      find: '',
      all: [],
      allClean: [],
      init() {
        this.all = [...this.$root.querySelectorAll('.csv-raw-cols input')].map((input) => input.value);
        this.allClean = [...this.$root.querySelectorAll('.csv-clean-cols input')].map((input) => input.value);
        const saved = readSaved('ll-csv-columns');
        const known = Array.isArray(saved) ? saved.filter((name) => this.all.includes(name)) : [];
        this.chosen = known.length ? known : [...this.all];
        const savedClean = readSaved('ll-csv-clean-columns');
        const knownClean = Array.isArray(savedClean) ? savedClean.filter((id) => this.allClean.includes(id)) : [];
        this.clean = knownClean.length ? knownClean : [...this.allClean];
        if (readSaved('ll-csv-style') === 'outscraper') this.style = 'outscraper';
      },
      essentials() { return (this.$root.dataset.essentials || '').split(',').filter(Boolean); },
      preset(name) {
        this.chosen = name === 'all' ? [...this.all] : name === 'essentials' ? this.essentials() : [];
      },
      is(name) {
        const want = name === 'all' ? this.all : this.essentials();
        return want.length === this.chosen.length && want.every((column) => this.chosen.includes(column));
      },
      groupFull(cols) { return cols.split(' ').every((column) => this.chosen.includes(column)); },
      toggleGroup(cols) {
        const list = cols.split(' ');
        this.chosen = this.groupFull(cols)
          ? this.chosen.filter((column) => !list.includes(column))
          : [...new Set([...this.chosen, ...list])];
      },
      cleanFull(cols) { return cols.trim().split(' ').every((id) => this.clean.includes(id)); },
      toggleClean(cols) {
        const list = cols.trim().split(' ');
        this.clean = this.cleanFull(cols)
          ? this.clean.filter((id) => !list.includes(id))
          : [...new Set([...this.clean, ...list])];
      },
      picked() { return this.style === 'clean' ? this.clean : this.chosen; },
      rowsNote() {
        if (this.style === 'clean') return 'One row per business. Extra emails go in “Other emails”.';
        return this.chosen.some((column) => column.startsWith('email')) ? 'A business with several emails gets one row per email.' : 'One row per business.';
      },
      url() {
        const params = new URLSearchParams(this.$root.dataset.filters || '');
        const every = this.style === 'clean' ? this.allClean : this.all;
        const ordered = every.filter((column) => this.picked().includes(column));
        if (this.style === 'outscraper') params.set('style', 'outscraper');
        if (ordered.length && ordered.length < every.length) params.set('columns', ordered.join(','));
        const query = params.toString();
        return `/leads/export.csv${query ? `?${query}` : ''}`;
      },
      save() {
        try {
          localStorage.setItem('ll-csv-style', JSON.stringify(this.style));
          localStorage.setItem('ll-csv-columns', JSON.stringify(this.chosen));
          localStorage.setItem('ll-csv-clean-columns', JSON.stringify(this.clean));
        } catch (e) { /* private mode */ }
      },
    }));
    Alpine.data('proxyTable', selection);
    Alpine.data('leadTable', selection);
  });

  // Live updates. One EventSource per tab; it survives hx-boost navigation
  // because it is owned by this script, not by page markup.
  let source = null;
  let retry = 1000;
  function connect() {
    source = new EventSource('/events');
    source.addEventListener('open', () => {
      retry = 1000;
      if (window.Alpine) Alpine.store('conn').online = true;
    });
    source.addEventListener('jobs', () => htmx.trigger(document.body, 'jobs-changed'));
    source.addEventListener('proxies', () => htmx.trigger(document.body, 'proxies-changed'));
    // Background enrichment saved leads: refresh rows in place, keeping
    // the page and selection.
    source.addEventListener('leads', () => htmx.trigger(document.body, 'leads-updated'));
    source.addEventListener('proxy-progress', () => htmx.trigger(document.body, 'proxy-progress'));
    source.addEventListener('update', () => htmx.trigger(document.body, 'update-changed'));
    source.addEventListener('resync', () => {
      htmx.trigger(document.body, 'jobs-changed');
      htmx.trigger(document.body, 'proxies-changed');
    });
    source.addEventListener('job-finished', (event) => {
      try { notifyFinished(JSON.parse(event.data)); } catch (e) { /* ignore malformed */ }
    });
    source.addEventListener('notice', (event) => {
      try {
        const notice = JSON.parse(event.data);
        toast(notice.level, notice.message, notice.undo);
      } catch (e) { /* ignore malformed */ }
    });
    source.addEventListener('error', () => {
      if (window.Alpine) Alpine.store('conn').online = false;
      if (source.readyState === EventSource.CLOSED) {
        setTimeout(connect, retry);
        retry = Math.min(retry * 2, 15000);
      }
    });
  }

  // Health legend on the Proxies page: click a status to filter by it,
  // click it again to clear the filter.
  window.filterProxyStatus = (status) => {
    const select = document.querySelector('#proxy-filters [name=status]');
    if (!select) return;
    select.value = select.value === status ? '' : status;
    select.dispatchEvent(new Event('change', { bubbles: true }));
  };

  // ---- Lead tables: DataTables in server-side mode. The browser holds one
  // page at a time; /leads/data filters, sorts and pages in SQLite, so the
  // table stays fast however many leads there are.
  const esc = (value) => String(value ?? '').replace(/[&<>"']/g, (c) => (
    { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));
  const dash = '<span class="muted">—</span>';
  const safeHref = (url) => (/^https?:\/\//i.test(url || '') ? url : '');

  // Optional lead columns, picked from the Columns menu and remembered in
  // this browser. Business, location, phone, website, email and rating
  // always show.
  const OPTIONAL_COLUMNS = [
    { name: 'num', label: 'Row number', on: false },
    { name: 'address', label: 'Street address', on: false },
    { name: 'status', label: 'Business status', on: false },
    { name: 'added', label: 'Date added', on: false },
  ];
  let columnPrefs = {};
  try { columnPrefs = JSON.parse(localStorage.getItem('ll-columns') || '{}') || {}; } catch (e) { columnPrefs = {}; }
  const columnOn = (name) => {
    const col = OPTIONAL_COLUMNS.find((c) => c.name === name);
    if (!col) return true;
    return name in columnPrefs ? !!columnPrefs[name] : col.on;
  };
  document.addEventListener('alpine:init', () => {
    Alpine.store('cols', {
      optional: OPTIONAL_COLUMNS,
      prefs: columnPrefs,
      on(name) { return name in this.prefs ? !!this.prefs[name] : columnOn(name); },
      set(name, on) {
        this.prefs[name] = on;
        columnPrefs = { ...this.prefs };
        try { localStorage.setItem('ll-columns', JSON.stringify(columnPrefs)); } catch (e) { /* private mode */ }
        document.querySelectorAll('table[data-leads-table]').forEach((el) => {
          if (el._dt) el._dt.column(name + ':name').visible(on);
        });
      },
    });
  });

  // Opens the CSV column chooser for a filter query string.
  window.llCsvColumns = (query) => {
    htmx.ajax('GET', `/leads/export/columns${query ? `?${query}` : ''}`, { target: '#modal-root', swap: 'innerHTML' });
  };

  // A cell's value with a small copy button beside it.
  const COPY_ICON = '<svg class="icon" viewBox="0 0 24 24" aria-hidden="true"><use href="#i-copy"/></svg>';
  function withCopy(html, value, what) {
    return `<span class="copy-cell">${html}<button type="button" class="cell-copy" data-copy-value="${esc(value)}" title="Copy ${what}" aria-label="Copy ${what}">${COPY_ICON}</button></span>`;
  }

  function leadColumns(select) {
    const columns = [];
    if (select) {
      columns.push({ data: 'id', name: 'check', orderable: false, className: 'w-check',
        title: '<input type="checkbox" aria-label="Select this page" :checked="pageSelected()" @change="togglePage($event.target.checked)">',
        render: (d) => `<input type="checkbox" aria-label="Select lead" data-id="${esc(d)}" :checked="isSelected($el.dataset.id)" @change="toggle($el.dataset.id)">` });
    }
    columns.push(
      { data: null, name: 'num', title: '#', orderable: false, className: 'num muted small w-num', render: (d, type, r, meta) => (meta.settings._iDisplayStart + meta.row + 1).toLocaleString() },
      { data: 'name', name: 'name', title: 'Business', className: 'cell-business', render: (d, type, r) => `<div class="strong clip-line" title="${esc(r.name)}">${esc(r.name)}</div><div class="muted small clip-line">${esc(r.category)}</div>` },
      { data: 'location', name: 'city', title: 'Location', className: 'small cell-location', render: (d, type, r) => (r.location
        ? `<span class="clip-line" title="${esc(r.address)}">${esc(r.location)}${r.postal_code ? ` <span class="muted">${esc(r.postal_code)}</span>` : ''}</span>`
        : `<span class="muted" title="${esc(r.address)}">—</span>`) },
      { data: 'address', name: 'address', title: 'Street address', orderable: false, className: 'small cell-address', render: (d) => (d ? `<span class="clip-line" title="${esc(d)}">${esc(d)}</span>` : dash) },
      { data: 'phone', name: 'phone', title: 'Phone', className: 'small nowrap cell-phone', render: (d) => (d ? withCopy(`<span class="clip-line">${esc(d)}</span>`, d, 'phone') : dash) },
      { data: 'website', name: 'website', title: 'Website', className: 'small cell-website', render: (d, type, r) => {
        const href = safeHref(d);
        if (!d) return dash;
        return withCopy(href ? `<a class="clip-line" href="${esc(href)}" target="_blank" rel="noopener noreferrer" hx-boost="false" title="${esc(d)}">${esc(r.website_host)}</a>` : `<span class="clip-line">${esc(d)}</span>`, d, 'website');
      } },
      { data: 'email', name: 'email', title: 'Best email', className: 'small cell-email', render: (d, type, r) => {
        if (!d) {
          if (!r.website) return dash;
          if (r.enrich === 'queued') return '<span class="muted">checking…</span>';
          if (r.enrich === 'done' || r.enrich === 'failed') return '<span class="muted">none found</span>';
          return '<span class="muted">not checked</span>';
        }
        const reach = r.email_reach;
        const label = { safe: 'Verified', risky: 'Risky', unknown: 'Unverified', invalid: 'Invalid' }[reach] || reach;
        const more = r.more_emails ? ` <span class="muted">+${r.more_emails}</span>` : '';
        const dot = reach ? `<span class="reach-dot ${esc(reach)}" title="${esc(label)}"></span>` : '';
        return withCopy(`<span class="email-line">${dot}<span class="clip-line" title="${esc(d)}${reach ? ' · ' + esc(label) : ''}">${esc(d)}</span>${more}</span>`, d, 'email');
      } },
      { data: 'rating', name: 'rating', title: 'Rating', className: 'num nowrap cell-rating', orderSequence: ['desc', 'asc'], render: (d, type, r) => (d
        ? `★ ${esc(d)}` + (r.reviews ? ` <span class="muted small">(${esc(r.reviews)})</span>` : '')
        : dash) },
      { data: 'status', name: 'status', title: 'Status', orderable: false, className: 'small muted cell-status', render: (d) => (d ? esc(d.toLowerCase().replace(/_/g, ' ')) : dash) },
      { data: 'added', name: 'added', title: 'Added', className: 'num muted small nowrap cell-added', orderSequence: ['desc', 'asc'], render: (d) => esc(d) },
    );
    columns.forEach((col) => {
      col.visible = columnOn(col.name);
      // Phones stack each row; the label says what each line is.
      const label = col.name === 'name' || col.name === 'check' ? '' : col.title.replace(/<[^>]+>/g, '');
      col.createdCell = (td) => { td.dataset.label = label; };
    });
    return columns;
  }

  // ---- Lead side panel: open a lead, step through the table with ↑/↓.
  let currentLead = null;
  const leadRows = () => Array.from(document.querySelectorAll('table[data-leads-table] tbody tr[data-id]'));
  const markCurrent = () => leadRows().forEach((row) => row.classList.toggle('current', row.dataset.id === currentLead));
  window.llOpenLead = (id) => {
    currentLead = id;
    markCurrent();
    htmx.ajax('GET', '/leads/' + encodeURIComponent(id), { target: '#modal-root', swap: 'innerHTML' });
  };
  window.llLeadClosed = () => { currentLead = null; markCurrent(); };
  window.llHasLeadRows = () => leadRows().length > 1;
  window.llStepLead = (dir) => {
    const rows = leadRows();
    const index = rows.findIndex((row) => row.dataset.id === currentLead);
    const next = rows[index + dir];
    if (next) {
      next.scrollIntoView({ block: 'nearest' });
      window.llOpenLead(next.dataset.id);
      return;
    }
    // Past the end of this page: turn the page and open its first (or last) lead.
    const table = rows[0] && rows[0].closest('table');
    const dt = table && table._dt;
    if (!dt) return;
    const info = dt.page.info();
    if ((dir > 0 && info.page >= info.pages - 1) || (dir < 0 && info.page <= 0)) return;
    dt.one('draw', () => {
      const fresh = leadRows();
      const target = dir > 0 ? fresh[0] : fresh[fresh.length - 1];
      if (target) window.llOpenLead(target.dataset.id);
    });
    dt.page(dir > 0 ? 'next' : 'previous').draw('page');
  };
  document.addEventListener('keydown', (event) => {
    if (!document.querySelector('#modal-root .drawer')) return;
    if (event.target.closest && event.target.closest('input, textarea, select, [contenteditable]')) return;
    if (event.key === 'ArrowDown' || event.key === 'j') { event.preventDefault(); window.llStepLead(1); }
    if (event.key === 'ArrowUp' || event.key === 'k') { event.preventDefault(); window.llStepLead(-1); }
  });

  function initLeadTable(el) {
    if (el._dt || !window.DataTable) return;
    const select = el.dataset.select !== '0';
    const state = {};
    const columns = leadColumns(select);
    const addedIndex = columns.findIndex((col) => col.name === 'added');
    const dt = new DataTable(el, {
      serverSide: true,
      processing: true,
      searching: false,
      autoWidth: false,
      pageLength: select ? 50 : 25,
      lengthMenu: [25, 50, 100, 250, 500, 1000],
      order: [[addedIndex, 'desc']],
      columns,
      layout: {
        topStart: null,
        topEnd: null,
        bottomStart: ['info', 'pageLength'],
        bottomEnd: 'paging',
      },
      language: {
        info: '_START_–_END_ of _TOTAL_',
        infoEmpty: 'No leads',
        infoFiltered: '',
        lengthMenu: '_MENU_ per page',
        emptyTable: select ? 'No leads match these filters.' : 'No leads yet. They show up here as the search finds them.',
        zeroRecords: 'No leads match these filters.',
        processing: '<div></div><div></div><div></div><div></div>',
        paginate: { first: '«', previous: '‹', next: '›', last: '»' },
      },
      createdRow: (row, data) => {
        row.classList.add('clickable');
        row.dataset.id = data.id;
        if (data.id === currentLead) row.classList.add('current');
        if (select) row.setAttribute(':class', "isSelected($el.dataset.id) && 'selected'");
      },
      ajax: (data, callback) => {
        fetchPage('/leads/data', el, data, callback, state, 'leads').then((json) => {
          const s = json && select && tableScope(el);
          if (!s || !s.pageIds) return;
          s.pageIds = json.data.map((row) => row.id);
          s.matching = json.recordsFiltered;
        });
      },
    });
    el._dt = dt;
    el.addEventListener('click', (event) => {
      if (event.target.closest('a, input, button, .w-check, thead')) return;
      const row = event.target.closest('tbody tr[data-id]');
      if (row) window.llOpenLead(row.dataset.id);
    });
  }

  // ---- Proxies table: same server-side mode. Selection lives in the
  // surrounding Alpine proxyTable() scope, so it survives paging and live
  // refreshes; the rows bind to it through Alpine attributes.
  function fetchPage(url, el, data, callback, state, noun) {
    const params = new URLSearchParams(el.dataset.params || '');
    const form = el.dataset.form && document.getElementById(el.dataset.form);
    if (form) for (const [key, value] of new FormData(form)) params.append(key, value);
    params.set('draw', data.draw);
    params.set('start', data.start);
    params.set('length', data.length);
    if (data.order && data.order.length) {
      const column = data.columns && data.columns[data.order[0].column];
      params.set('order_col', data.order[0].column);
      params.set('order_dir', data.order[0].dir);
      if (column && column.name) {
        params.set('order_key', column.name);
        el.dataset.sort = column.name + ':' + data.order[0].dir;
      }
    }
    // Only the newest request matters when filters change quickly.
    if (state.controller) state.controller.abort();
    state.controller = new AbortController();
    return fetch(url + '?' + params, { signal: state.controller.signal, headers: { Accept: 'application/json' } })
      .then((response) => (response.ok ? response.json() : Promise.reject(new Error(response.statusText))))
      .then((json) => { callback(json); return json; })
      .catch((error) => {
        if (error.name === 'AbortError') return null;
        toast('error', `Could not load ${noun}: ${error.message}`);
        callback({ draw: data.draw, recordsTotal: 0, recordsFiltered: 0, data: [] });
        return null;
      });
  }

  function tableScope(el) {
    const host = el.closest('[x-data]');
    return window.Alpine && host ? Alpine.$data(host) : null;
  }

  function initProxyTable(el) {
    if (el._dt || !window.DataTable) return;
    const state = {};
    const scope = () => tableScope(el);
    const dt = new DataTable(el, {
      serverSide: true,
      processing: true,
      searching: false,
      autoWidth: false,
      pageLength: 100,
      lengthMenu: [50, 100, 250, 500, 1000],
      order: [],
      columns: [
        { data: 'id', orderable: false, className: 'w-check', render: (d) => `<input type="checkbox" aria-label="Select proxy" data-id="${esc(d)}" :checked="isSelected($el.dataset.id)" @change="toggle($el.dataset.id)">` },
        { data: 'masked', className: 'mono small clip-wide', render: (d, type, r) => esc(d) + (r.enabled ? '' : ' <span class="tag">off</span>') },
        { data: 'status', render: (d, type, r) => `<span class="badge ${esc(r.status_class)}">${esc(d)}</span>` },
        { data: 'latency', className: 'num small', render: (d) => esc(d) },
        { data: 'groups', orderable: false, className: 'small clip muted', render: (d) => esc(d) },
        { data: 'last_checked', className: 'small nowrap', orderSequence: ['desc', 'asc'], render: (d, type, r) => `<button type="button" class="link quiet" data-history="${esc(r.id)}" title="Show check history">${d ? esc(d) : 'never'}</button>` },
        { data: 'last_error', className: 'small muted clip', render: (d) => `<span title="${esc(d)}">${esc(d)}</span>` },
      ],
      layout: {
        topStart: null,
        topEnd: null,
        bottomStart: ['info', 'pageLength'],
        bottomEnd: 'paging',
      },
      language: {
        info: '_START_–_END_ of _TOTAL_',
        infoEmpty: 'No proxies',
        infoFiltered: '',
        lengthMenu: '_MENU_ per page',
        emptyTable: 'No proxies here yet. <button type="button" class="btn primary sm" onclick="window.dispatchEvent(new CustomEvent(\'open-dialog\', { detail: \'add\' }))">Add proxies</button>',
        zeroRecords: 'No proxies match these filters.',
        processing: '<div></div><div></div><div></div><div></div>',
        paginate: { first: '«', previous: '‹', next: '›', last: '»' },
      },
      createdRow: (row, data) => {
        row.dataset.id = data.id;
        row.setAttribute(':class', "isSelected($el.dataset.id) && 'selected'");
      },
      ajax: (data, callback) => {
        fetchPage('/proxies/data', el, data, callback, state, 'proxies').then((json) => {
          const s = json && scope();
          if (!s) return;
          s.pageIds = json.data.map((row) => row.id);
          s.matching = json.recordsFiltered;
        });
      },
    });
    el._dt = dt;
    el.querySelector('tbody').addEventListener('click', (event) => {
      const button = event.target.closest('[data-history]');
      if (button) htmx.ajax('GET', '/proxies/' + encodeURIComponent(button.dataset.history) + '/history', { target: '#modal-root' });
    });
  }

  const leadTables = (root) => (root || document).querySelectorAll('table[data-leads-table], table[data-proxy-table]');
  const initTable = (el) => {
    if (el.hasAttribute('data-proxy-table')) initProxyTable(el); else initLeadTable(el);
    // The rows scroll inside their own box, under a pinned header.
    const box = el.closest('.dt-layout-cell');
    if (!el._dt || !box || box.classList.contains('table-scroll')) return;
    box.classList.add('table-scroll');
    el._dt.on('page', () => { box.scrollTop = 0; });
    el._dt.on('draw', fitTables);
    fitTables();
  };

  // Each table scrolls in its own box, sized so the box and the pager under
  // it end at the bottom of the window (never shorter than 320px). Phones
  // keep scrolling the page: a scroll box inside a page is awkward on touch.
  let fitFrame = 0;
  function fitTables() {
    if (fitFrame) return;
    // A timer, not requestAnimationFrame: that never fires in a background tab.
    fitFrame = setTimeout(() => {
      fitFrame = 0;
      const content = document.querySelector('.content');
      const phone = window.innerWidth <= 640;
      document.querySelectorAll('.table-scroll').forEach((box) => {
        if (phone || !content || !box.offsetParent) {
          box.style.removeProperty('--fit');
          return;
        }
        const top = box.getBoundingClientRect().top + window.scrollY;
        const bottom = top + box.offsetHeight;
        const contentBottom = content.getBoundingClientRect().bottom + window.scrollY;
        // What sits under the box: the pager, panel padding and the page's
        // own bottom padding, so the page itself doesn't scroll.
        const below = Math.max(0, contentBottom - bottom);
        const fit = Math.max(320, Math.floor(window.innerHeight - top - below)) + 'px';
        if (box.style.getPropertyValue('--fit') !== fit) box.style.setProperty('--fit', fit);
      });
    }, 30);
  }
  window.addEventListener('resize', fitTables);
  // Live refreshes replace a table's box; keep where it was scrolled to.
  const savedScroll = {};
  document.addEventListener('htmx:beforeSwap', (event) => {
    const target = event.detail.target;
    if (!target || !target.querySelectorAll) return;
    target.querySelectorAll('[data-scroll-key]').forEach((box) => { savedScroll[box.dataset.scrollKey] = box.scrollTop; });
  });
  document.addEventListener('htmx:afterSettle', (event) => {
    const target = event.detail.target;
    // A new page starts at the top; only live refreshes keep the scroll.
    if (target && target !== document.body && target.querySelectorAll) {
      target.querySelectorAll('[data-scroll-key]').forEach((box) => {
        const top = savedScroll[box.dataset.scrollKey];
        if (top) box.scrollTop = top;
      });
    }
    fitTables();
  });
  // Filters changed outside htmx: back to page 1.
  window.llReloadTable = (selector) => reloadLeadTables(selector, false);
  // keepPage: stay on the current page (live refresh); otherwise go to page 1
  // (filters changed).
  function reloadLeadTables(selector, keepPage) {
    document.querySelectorAll(selector).forEach((el) => {
      if (!el._dt) return;
      const s = !keepPage && tableScope(el);
      if (s && s.clear) s.clear();
      el._dt.ajax.reload(null, !keepPage);
    });
  }
  function destroyLeadTables(root) {
    leadTables(root).forEach((el) => {
      if (!el._dt) return;
      el._dt.destroy();
      el._dt = null;
      const body = el.querySelector('tbody');
      if (body) body.remove();
    });
  }

  document.addEventListener('DOMContentLoaded', () => {
    connect();

    // htmx may have processed the page before this ran, so set up now too.
    leadTables().forEach(initTable);
    htmx.onLoad((root) => leadTables(root).forEach(initTable));
    fitTables();
    // Filters opening or closing above a table move it: refit.
    const content = document.querySelector('.content');
    if (content && window.ResizeObserver) new ResizeObserver(fitTables).observe(content);
    document.body.addEventListener('htmx:historyRestore', () => leadTables().forEach(initTable));
    // Lead tables carry hx-history="false", so htmx refetches those pages on
    // Back instead of restoring a snapshot. Tear tables down before a boosted
    // page swap so DataTables lets go of them.
    document.body.addEventListener('htmx:beforeSwap', (event) => {
      if (event.detail.target === document.body) destroyLeadTables(document.body);
    });
    // Filters changed (the facet request went through): show the matching
    // rows from page 1.
    document.body.addEventListener('htmx:afterRequest', (event) => {
      const elt = event.detail.elt;
      if (event.detail.successful && elt && elt.id) reloadLeadTables(`table[data-form="${CSS.escape(elt.id)}"]`, false);
    });
    let liveTimer = null;
    const liveReload = () => {
      if (liveTimer) return;
      liveTimer = setTimeout(() => { liveTimer = null; reloadLeadTables('table[data-live]', true); }, 2000);
    };
    document.body.addEventListener('jobs-changed', liveReload);
    let proxyTimer = null;
    document.body.addEventListener('proxies-changed', () => {
      if (proxyTimer) return;
      proxyTimer = setTimeout(() => { proxyTimer = null; reloadLeadTables('table[data-proxy-table]', true); }, 1000);
    });
    document.body.addEventListener('leads-changed', () => reloadLeadTables('table[data-leads-table]:not([data-form])', true));
    let updateTimer = null;
    document.body.addEventListener('leads-updated', () => {
      if (updateTimer) return;
      updateTimer = setTimeout(() => { updateTimer = null; reloadLeadTables('table[data-leads-table]', true); }, 1500);
    });

    // Toasts requested by response headers (HX-Trigger: {"notice": {...}}).
    document.body.addEventListener('notice', (event) => {
      const detail = event.detail || {};
      toast(detail.level, detail.message, detail.undo);
    });

    document.body.addEventListener('htmx:responseError', (event) => {
      const xhr = event.detail.xhr;
      const text = (xhr && xhr.responseText) || 'Request failed';
      toast('error', text.length > 300 ? text.slice(0, 300) + '…' : text);
    });

    document.body.addEventListener('htmx:sendError', () => {
      toast('error', 'Cannot reach the LocalLeads server');
    });

    // Copy buttons on code snippets ([data-copy] copies the snippet's code;
    // [data-copy-text] copies the element's own text; [data-copy-line]
    // copies its log line; [data-copy-value] copies that attribute).
    document.addEventListener('click', (event) => {
      const line = event.target.closest('[data-copy-line]');
      if (line) {
        const li = line.closest('li');
        if (li) copyText(logLine(li), () => flashCopied(line));
        return;
      }
      const value = event.target.closest('[data-copy-value]');
      if (value) {
        event.preventDefault();
        event.stopPropagation();
        copyText(value.dataset.copyValue, () => flashCopied(value));
        return;
      }
      const button = event.target.closest('[data-copy], [data-copy-text]');
      if (!button) return;
      const source = button.hasAttribute('data-copy')
        ? button.closest('.snippet')?.querySelector('code')
        : button;
      if (!source) return;
      copyText(source.innerText.trim(), () => {
        if (!button.hasAttribute('data-copy')) toast('ok', 'Copied');
        else {
          const label = button.textContent;
          button.textContent = 'Copied';
          setTimeout(() => { button.textContent = label; }, 1400);
        }
      });
    });

  });
})();
