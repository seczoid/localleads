// LocalLeads client glue: Alpine stores, live updates over SSE, and htmx hooks.
(function () {
  'use strict';

  let toastSeq = 0;
  const recent = new Map();

  function toast(level, message) {
    if (!message || !window.Alpine) return;
    // The same notice can arrive both as a response header and over SSE.
    const key = level + '|' + message;
    const now = Date.now();
    if (recent.has(key) && now - recent.get(key) < 2500) return;
    recent.set(key, now);
    Alpine.store('toasts').push(level, message);
  }
  window.llToast = toast;

  document.addEventListener('alpine:init', () => {
    Alpine.store('toasts', {
      items: [],
      push(level, message) {
        const id = ++toastSeq;
        this.items.push({ id, level: level || 'info', message: String(message).slice(0, 400) });
        if (this.items.length > 5) this.items.shift();
        setTimeout(() => this.dismiss(id), level === 'error' ? 9000 : 4500);
      },
      dismiss(id) {
        this.items = this.items.filter((t) => t.id !== id);
      },
    });

    Alpine.store('conn', { online: false });

    Alpine.store('theme', {
      toggle() {
        const root = document.documentElement;
        const dark = root.dataset.theme
          ? root.dataset.theme === 'dark'
          : window.matchMedia('(prefers-color-scheme: dark)').matches;
        root.dataset.theme = dark ? 'light' : 'dark';
        try { localStorage.setItem('ll-theme', root.dataset.theme); } catch (e) { /* private mode */ }
      },
    });

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
    Alpine.data('regionPicker', (initial, custom) => ({
      country: initial.country,
      expandable: initial.expandable,
      codes: initial.codes || {},
      states: [],
      custom: !!custom,
      q: '',
      results: [],
      init() {
        this.setStates(initial.states, initial.all, initial.picked_states, initial.picked_cities);
      },
      setStates(names, all, pickedStates, pickedCities) {
        const full = new Set(pickedStates || []);
        const byCode = {};
        this.states = (names || []).map((name) => {
          const state = { name, full: !!all || full.has(name), open: false, loading: false, cities: null, picked: new Set() };
          if (this.codes[name]) byCode[this.codes[name]] = state;
          return state;
        });
        (pickedCities || []).forEach((label) => {
          const code = label.split(',').pop().trim().toUpperCase();
          const state = byCode[code];
          if (state && !state.full) state.picked.add(label);
        });
      },
      async loadCountry() {
        this.q = '';
        this.results = [];
        try {
          const response = await fetch('/locations/regions?country=' + encodeURIComponent(this.country));
          const data = await response.json();
          this.expandable = data.expandable;
          this.codes = data.codes || {};
          this.setStates(data.states, true, [], []);
        } catch (e) {
          window.llToast && window.llToast('error', 'Could not load places for ' + this.country);
        }
        this.changed();
      },
      changed() {
        this.$nextTick(() => this.$root.dispatchEvent(new Event('change', { bubbles: true })));
      },
      async loadCities(state) {
        if (state.cities || !this.expandable) return;
        state.loading = true;
        try {
          const response = await fetch('/locations/cities?state=' + encodeURIComponent(state.name));
          state.cities = await response.json();
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
        try {
          const response = await fetch('/locations/cities?q=' + encodeURIComponent(q));
          this.results = q === this.q.trim() ? await response.json() : this.results;
        } catch (e) {
          this.results = [];
        }
      },
      visibleStates() {
        const q = this.q.trim().toLowerCase();
        return q ? this.states.filter((s) => s.name.toLowerCase().includes(q)) : this.states;
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
          return [{ key: 'all', label: `All ${this.country} locations (${this.states.length})`, remove: () => this.clearAll() }];
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
        if (states) parts.push(states + (states === 1 ? ' state' : ' states'));
        if (cities) parts.push(cities + (cities === 1 ? ' city' : ' cities'));
        return parts.length ? parts.join(', ') + ' selected' : 'Nothing selected';
      },
      zipText(city) {
        return city.zips > 1 ? `${city.zips} ZIP codes` : '';
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
    source.addEventListener('resync', () => {
      htmx.trigger(document.body, 'jobs-changed');
      htmx.trigger(document.body, 'proxies-changed');
    });
    source.addEventListener('notice', (event) => {
      try {
        const notice = JSON.parse(event.data);
        toast(notice.level, notice.message);
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

  function leadColumns(compact) {
    const columns = compact ? [] : [
      { data: 'id', orderable: false, className: 'w-check', render: (d) => `<input type="checkbox" aria-label="Select lead" data-id="${esc(d)}" :checked="isSelected($el.dataset.id)" @change="toggle($el.dataset.id)">` },
      { data: null, orderable: false, className: 'num muted small w-num', render: (d, type, r, meta) => (meta.settings._iDisplayStart + meta.row + 1).toLocaleString() },
    ];
    columns.push(
      { data: 'name', name: 'name', render: (d, type, r) => `<div class="strong clip" title="${esc(r.name)}">${esc(r.name)}</div><div class="muted small clip">${esc(r.category)}</div>` },
      { data: 'location', name: 'city', className: 'small', render: (d, type, r) => (r.location
        ? esc(r.location) + (!compact && r.postal_code ? ` <span class="muted">${esc(r.postal_code)}</span>` : '')
        : `<span class="muted" title="${esc(r.address)}">—</span>`) },
      { data: 'phone', name: 'phone', className: 'small nowrap', render: (d) => (d ? esc(d) : dash) },
      { data: 'website', name: 'website', className: 'small clip', render: (d, type, r) => {
        const href = safeHref(d);
        if (!d) return dash;
        return href ? `<a href="${esc(href)}" target="_blank" rel="noopener noreferrer" hx-boost="false">${esc(r.website_host)}</a>` : esc(d);
      } },
      { data: 'email', name: 'email', className: 'small clip email-cell', render: (d, type, r) => {
        if (!d) {
          if (!r.website) return dash;
          if (r.enrich === 'queued') return '<span class="muted">checking…</span>';
          if (r.enrich === 'done' || r.enrich === 'failed') return '<span class="muted">none found</span>';
          return dash;
        }
        const reach = r.email_reach;
        const label = { safe: 'Verified', risky: 'Risky', unknown: 'Unverified', invalid: 'Invalid' }[reach] || reach;
        const more = r.more_emails ? ` <span class="muted">+${r.more_emails}</span>` : '';
        if (!reach) return `<span title="${esc(d)}">${esc(d)}</span>` + more;
        return `<span class="reach-dot ${esc(reach)}" title="${esc(label)}"></span><span title="${esc(d)} · ${esc(label)}">${esc(d)}</span>` + more;
      } },
      { data: 'rating', name: 'rating', className: 'num nowrap', orderSequence: ['desc', 'asc'], render: (d, type, r) => (d
        ? `★ ${esc(d)}` + (r.reviews ? ` <span class="muted small">(${esc(r.reviews)})</span>` : '')
        : '') },
    );
    if (!compact) columns.push({ data: 'added', name: 'added', className: 'num muted small nowrap', orderSequence: ['desc', 'asc'], render: (d) => esc(d) });
    return columns;
  }

  function initLeadTable(el) {
    if (el._dt || !window.DataTable) return;
    const compact = el.dataset.compact === '1';
    const state = {};
    const dt = new DataTable(el, {
      serverSide: true,
      processing: true,
      searching: false,
      autoWidth: false,
      pageLength: compact ? 25 : 50,
      lengthMenu: [25, 50, 100, 250, 500, 1000],
      order: compact ? [] : [[8, 'desc']],
      columns: leadColumns(compact),
      layout: {
        topStart: compact ? null : 'info',
        topEnd: compact ? null : ['pageLength', 'paging'],
        bottomStart: 'info',
        bottomEnd: 'paging',
      },
      language: {
        info: '_START_–_END_ of _TOTAL_',
        infoEmpty: 'No leads',
        infoFiltered: '',
        lengthMenu: '_MENU_ per page',
        emptyTable: compact ? 'No leads yet.' : 'No leads match these filters.',
        zeroRecords: 'No leads match these filters.',
        processing: '<div></div><div></div><div></div><div></div>',
        paginate: { first: '«', previous: '‹', next: '›', last: '»' },
      },
      createdRow: (row, data) => {
        row.classList.add('clickable');
        row.dataset.id = data.id;
        if (!compact) row.setAttribute(':class', "isSelected($el.dataset.id) && 'selected'");
      },
      ajax: (data, callback) => {
        fetchPage('/leads/data', el, data, callback, state, 'leads').then((json) => {
          const s = json && !compact && tableScope(el);
          if (!s || !s.pageIds) return;
          s.pageIds = json.data.map((row) => row.id);
          s.matching = json.recordsFiltered;
        });
      },
    });
    el._dt = dt;
    el.querySelector('tbody').addEventListener('click', (event) => {
      if (event.target.closest('a, input, .w-check')) return;
      const row = event.target.closest('tr[data-id]');
      if (row) htmx.ajax('GET', '/leads/' + encodeURIComponent(row.dataset.id), { target: '#modal-root' });
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
  const initTable = (el) => (el.hasAttribute('data-proxy-table') ? initProxyTable(el) : initLeadTable(el));
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
      toast(detail.level, detail.message);
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
    // [data-copy-text] copies the element's own text).
    document.addEventListener('click', (event) => {
      const button = event.target.closest('[data-copy], [data-copy-text]');
      if (!button) return;
      const source = button.hasAttribute('data-copy')
        ? button.closest('.snippet')?.querySelector('code')
        : button;
      if (!source) return;
      const text = source.innerText.trim();
      const done = () => {
        if (!button.hasAttribute('data-copy')) toast('ok', 'Copied');
        else {
          const label = button.textContent;
          button.textContent = 'Copied';
          setTimeout(() => { button.textContent = label; }, 1400);
        }
      };
      if (navigator.clipboard && window.isSecureContext) {
        navigator.clipboard.writeText(text).then(done, () => toast('error', 'Copy failed; select the text instead'));
      } else {
        const area = document.createElement('textarea');
        area.value = text;
        document.body.appendChild(area);
        area.select();
        try { document.execCommand('copy'); done(); } catch (e) { toast('error', 'Copy failed; select the text instead'); }
        area.remove();
      }
    });

  });
})();
