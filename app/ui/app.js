// Talks to the background agent through the Tauri command `agent_call`. No business logic lives here.
const T = window.__TAURI__;
const call = (method, params = {}) => T.core.invoke('agent_call', { method, params });
const $ = (s, r = document) => r.querySelector(s);
const esc = s => String(s ?? '').replace(/[&<>"]/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]));
const fmtTime = s => s ? new Date(s * 1000).toLocaleString([], { month: 'short', day: 'numeric', hour: 'numeric', minute: '2-digit' }) : '—';
const fmtBytes = n => n > 1e9 ? (n / 1e9).toFixed(1) + ' GB' : n > 1e6 ? (n / 1e6).toFixed(1) + ' MB' : n > 1e3 ? (n / 1e3).toFixed(0) + ' KB' : n + ' B';
const isWin = /Windows/.test(navigator.userAgent);
const base = p => p.split(/[\\/]/).filter(Boolean).pop() || 'folder';
const schedText = s => ({ manual: 'Run now only', every_hours: `Every ${s.hours} h`, daily: `Daily at ${String(s.hour).padStart(2, '0')}:${String(s.minute).padStart(2, '0')}`, weekly: `Weekly at ${String(s.hour).padStart(2, '0')}:${String(s.minute).padStart(2, '0')}` }[s.kind]);

function toast(msg, bad) {
  let el = $('#toast'); if (!el) { el = document.createElement('div'); el.id = 'toast'; document.body.appendChild(el); }
  el.textContent = msg; el.className = bad ? 'bad' : 'good'; el.style.display = 'block';
  clearTimeout(el._t); el._t = setTimeout(() => { el.style.display = 'none'; }, bad ? 12000 : 4000);
  el.onclick = () => { el.style.display = 'none'; };
}

let view = 'jobs', detailId = null, timer = null;

function statusPill(j) {
  const r = j.last_run;
  if (j.running) return '<span class="pill run">Running</span>';
  if (j.job.paused) return '<span class="pill idle">Paused</span>';
  if (!r) return '<span class="pill idle">Not run yet</span>';
  if (r.status === 'ok') return '<span class="pill ok">Backed up</span>';
  if (r.status === 'warnings') return `<span class="pill warn">${r.stats.warnings} warnings</span>`;
  if (r.status === 'failed') return '<span class="pill bad-p">Needs attention</span>';
  return `<span class="pill idle">${esc(r.status)}</span>`;
}

async function render() {
  document.querySelectorAll('nav a').forEach(a => a.classList.toggle('on', a.dataset.v === view));
  try {
    $('#agent').textContent = 'Agent running';
    const jobs = await call('list_jobs');
    const pending = jobs.reduce((n, j) => n + j.pending_deleted, 0);
    const b = $('#badge'); b.hidden = !pending; b.textContent = pending;
    if (view === 'jobs') return detailId ? await renderDetail(detailId) : renderJobs(jobs, pending);
    if (view === 'deleted') return await renderDeleted();
    if (view === 'settings') return await renderSettings();
  } catch (e) {
    $('#agent').textContent = 'Agent not running';
    $('#main').innerHTML = `<h1>Backer</h1><div class="banner">${esc(e)}</div>`;
  }
}

function renderJobs(jobs, pending) {
  const rows = jobs.map(j => `<tr>
    <td><a href="#" data-open="${j.job.id}"><b>${esc(j.job.name)}</b></a></td>
    <td>${statusPill(j)}</td><td>${schedText(j.job.schedule)}</td>
    <td>${j.running ? '—' : fmtTime(j.next_run)}</td>
    <td style="text-align:right"><button class="link" data-run="${j.job.id}">Run now</button>
      <button class="link" data-pause="${j.job.id}" data-p="${j.job.paused}">${j.job.paused ? 'Resume' : 'Pause'}</button></td></tr>`).join('');
  $('#main').innerHTML = `<div class="row"><div style="flex:1"><h1>Jobs</h1><div class="sub">Backups of folders on this Mac to your NAS.</div></div><button class="pri" id="new">New job</button></div>
    ${pending ? `<div class="banner">${pending} files were deleted from your Mac. They are still on the NAS. <a href="#deleted" data-go="deleted">Review</a></div>` : ''}
    ${jobs.length ? `<table><thead><tr><th>Name</th><th>Status</th><th>Schedule</th><th>Next run</th><th></th></tr></thead><tbody>${rows}</tbody></table>` : '<div class="card">No backup jobs yet. Create one to get started.</div>'}`;
  $('#new').onclick = () => wizard();
}

async function renderDetail(id) {
  const [j, runs] = await Promise.all([call('get_job', { job_id: id }), call('list_runs', { job_id: id, limit: 15 })]);
  const rr = runs.map(r => `<tr><td>${fmtTime(r.started)}</td><td>${r.stats.copied}</td><td>${r.status === 'ok' ? '<span class="pill ok">OK</span>' : r.status === 'failed' ? `<span class="pill bad-p">Failed</span> ${esc(r.message || '')}` : `<span class="pill warn">${esc(r.status)}</span>`}</td><td><button class="link" data-log="${r.id}">Log</button></td></tr>`).join('');
  $('#main').innerHTML = `<div class="sub"><a href="#" data-back>Jobs</a> / ${esc(j.job.name)}</div>
    <div class="row"><h1 style="flex:1">${esc(j.job.name)} ${statusPill(j)}</h1><button data-test="${id}">Test NAS</button><button class="pri" data-run="${id}">Run now</button></div>
    <div class="card"><div class="sub">Folders</div>${j.job.folders.map(f => `<div class="mono">${esc(f.source)} → ${esc(f.dest)}</div>`).join('')}<div class="sub" style="margin-top:8px">Schedule: ${schedText(j.job.schedule)} · Next: ${fmtTime(j.next_run)}</div></div>
    <h2>Run history</h2><table><thead><tr><th>Started</th><th>Copied</th><th>Result</th><th></th></tr></thead><tbody>${rr}</tbody></table><div id="log"></div>`;
}

async function renderDeleted() {
  const list = await call('list_deleted');
  $('#main').innerHTML = `<h1>Deleted files</h1><div class="sub">Deleted on your Mac but still on the NAS. Nothing is removed until you say so.</div>
    ${list.length ? `<table><thead><tr><th></th><th>File</th><th>Detected</th><th>Size</th></tr></thead><tbody>${list.map(d => `<tr><td><input type="checkbox" style="width:auto;height:auto" data-job="${d.job_id}" value="${esc(d.dest_path)}"></td><td class="mono">${esc(d.dest_path)}</td><td>${fmtTime(d.detected)}</td><td>${fmtBytes(d.size)}</td></tr>`).join('')}</tbody></table>
    <div class="row" style="margin-top:12px"><button id="keep">Keep on NAS</button><button class="bad" id="rm">Remove from NAS…</button></div>` : '<div class="card">Nothing to review.</div>'}`;
  const picked = () => [...document.querySelectorAll('#main input:checked')].map(i => ({ job: +i.dataset.job, path: i.value }));
  const byJob = ps => ps.reduce((m, p) => ((m[p.job] ||= []).push(p.path), m), {});
  if ($('#keep')) $('#keep').onclick = async () => { for (const [job, paths] of Object.entries(byJob(picked()))) await call('keep_deleted', { job_id: +job, paths }); render(); };
  if ($('#rm')) $('#rm').onclick = () => {
    const ps = picked(); if (!ps.length) return;
    const d = $('#dlg');
    d.innerHTML = `<h1>Remove ${ps.length} file${ps.length > 1 ? 's' : ''} from the NAS?</h1><p>They will be permanently deleted from the NAS. This cannot be undone and there are no earlier versions.</p>
      <div class="card mono">${ps.slice(0, 5).map(p => esc(p.path)).join('<br>')}${ps.length > 5 ? `<br>…and ${ps.length - 5} more` : ''}</div>
      <div class="row" style="justify-content:flex-end"><button id="c">Cancel</button><button class="bad" id="go">Remove ${ps.length}</button></div>`;
    d.showModal(); $('#c', d).onclick = () => d.close();
    $('#go', d).onclick = async () => { for (const [job, paths] of Object.entries(byJob(ps))) await call('remove_deleted', { job_id: +job, paths }); d.close(); render(); };
  };
}

async function renderSettings() {
  const clients = await call('list_mcp_clients');
  const cfg = await T.core.invoke('mcp_config').catch(() => '');
  $('#main').innerHTML = `<h1>Settings</h1><h2>AI assistants (MCP)</h2>
    <div class="sub">Approved assistants can see jobs, run them, pause them and change schedules. They cannot delete files or change what is backed up.</div>
    ${clients.length ? `<table><thead><tr><th>Program</th><th>Status</th><th>Last used</th><th></th></tr></thead><tbody>${clients.map(c => `<tr><td>${esc(c.client.name)}<div class="mono" style="color:var(--ink-muted)">${esc(c.identity.slice(0, 60))}</div></td><td>${c.client.approved ? '<span class="pill ok">Approved</span>' : '<span class="pill warn">Waiting for you</span>'}</td><td>${fmtTime(c.client.last_used)}</td>
      <td style="text-align:right">${c.client.approved ? '' : `<button class="pri" data-approve="${esc(c.identity)}">Allow</button> `}<button data-revoke="${esc(c.identity)}">${c.client.approved ? 'Revoke' : 'Deny'}</button></td></tr>`).join('')}</tbody></table>` : '<div class="card">No assistant has tried to connect.</div>'}
    <h2>Connect an assistant</h2>
    <div class="sub">Add this to your assistant's MCP settings (for Claude Desktop: Settings, Developer, Edit Config), then restart it. It will appear above, waiting for you to allow it.</div>
    <pre class="card mono" style="white-space:pre-wrap;user-select:all">${esc(cfg)}</pre>
    <button id="copycfg">Copy</button>`;
  const cb = $('#copycfg'); if (cb) cb.onclick = async () => { try { await navigator.clipboard.writeText(cfg); toast('Copied.'); } catch { toast('Select the text and copy it.', true); } };
}

// ---- New job wizard (5 steps in one dialog)
function wizard() {
  const w = { name: '', folders: [], dest_root: '', dest_base: 'mac', sched: 'daily', hour: 2, minute: 0, hours: 6, days: [1, 2, 3, 4, 5, 6, 7], catch_up: true, max_gb: '' };
  let step = 0; const names = ['Folders', 'NAS', 'Schedule', 'Options', 'Review'];
  const d = $('#dlg'); let error = '';
  const draw = () => {
    const body = [
      `<h1>Which folders do you want to back up?</h1>${w.folders.map((f, i) => `<div class="card row"><span class="mono" style="flex:1">${esc(f)}</span><button class="link" data-rm="${i}">Remove</button></div>`).join('')}<button id="add">Add folder…</button>`,
      `<h1>Where should the backup go?</h1><div class="sub">${isWin ? 'Connect to your NAS share in File Explorer first (for example map it to a drive letter, or open \\\\nas\\share), then choose its folder.' : 'Mount your NAS share in Finder first (Go ▸ Connect to Server), then choose its folder.'}</div><div class="row"><input id="root" class="mono" readonly value="${esc(w.dest_root)}" placeholder="${isWin ? 'Z:\\\\backups' : '/Volumes/backups'}"><button id="pick">Choose…</button></div><label>Folder on the NAS for these backups</label><input id="base" value="${esc(w.dest_base)}"><div class="sub" style="margin-top:8px">New destinations are marked with a small file so the app never writes to an unmounted share.</div>`,
      `<h1>When should it run?</h1><label>Schedule</label><select id="sched">${[['manual', 'Only when I click Run now'], ['every_hours', 'Every few hours'], ['daily', 'Every day'], ['weekly', 'Certain days']].map(([v, t]) => `<option value="${v}" ${w.sched === v ? 'selected' : ''}>${t}</option>`).join('')}</select>
        ${w.sched === 'every_hours' ? `<label>Hours</label><input id="hours" type="number" min="1" max="168" value="${w.hours}">` : ''}
        ${['daily', 'weekly'].includes(w.sched) ? `<label>Time</label><input id="time" type="time" value="${String(w.hour).padStart(2, '0')}:${String(w.minute).padStart(2, '0')}">` : ''}
        ${w.sched === 'weekly' ? `<label>Days</label><div class="row">${['Mon', 'Tue', 'Wed', 'Thu', 'Fri', 'Sat', 'Sun'].map((n, i) => `<label style="margin:0"><input type="checkbox" style="width:auto;height:auto" data-day="${i + 1}" ${w.days.includes(i + 1) ? 'checked' : ''}> ${n}</label>`).join('')}</div>` : ''}
        <label style="margin-top:12px"><input id="catch" type="checkbox" style="width:auto;height:auto" ${w.catch_up ? 'checked' : ''}> Run at the next opportunity if the Mac was asleep</label>`,
      `<h1>A few options</h1><label>Skip files larger than (GB, blank for no limit)</label><input id="max" type="number" min="1" value="${esc(w.max_gb)}"><div class="card">Files you delete on this Mac stay on the NAS. The app lists them and nothing is removed until you confirm.</div>`,
      `<h1>Ready to start</h1><label>Name</label><input id="name" value="${esc(w.name || w.folders.map(f => base(f)).join(', '))}"><div class="card">${w.folders.map(f => `<div class="mono">${esc(f)} → ${esc(w.dest_base)}/${esc(base(f))}</div>`).join('')}<div class="sub" style="margin-top:6px">Destination: ${esc(w.dest_root)} · ${w.sched === 'manual' ? 'Run now only' : w.sched}</div></div><label><input id="first" type="checkbox" style="width:auto;height:auto" checked> Start the first backup now</label>`,
    ][step];
    d.innerHTML = `<div class="steps">${names.map((n, i) => i === step ? `<b>${i + 1} ${n}</b>` : `<span>${i + 1} ${n}</span>`).join('<span>›</span>')}</div>${body}<div class="err">${esc(error)}</div>
      <div class="row" style="justify-content:flex-end;margin-top:16px"><button id="cancel">Cancel</button>${step ? '<button id="back">Back</button>' : ''}<button class="pri" id="next">${step === 4 ? 'Create job' : 'Continue'}</button></div>`;
    $('#cancel', d).onclick = () => d.close();
    if ($('#back', d)) $('#back', d).onclick = () => { read(); step--; error = ''; draw(); };
    $('#next', d).onclick = next;
    if ($('#add', d)) $('#add', d).onclick = async () => { const p = await T.dialog.open({ directory: true, multiple: true }); if (p) { [].concat(p).forEach(x => !w.folders.includes(x) && w.folders.push(x)); draw(); } };
    d.querySelectorAll('[data-rm]').forEach(b => b.onclick = () => { w.folders.splice(+b.dataset.rm, 1); draw(); });
    if ($('#pick', d)) $('#pick', d).onclick = async () => { const p = await T.dialog.open({ directory: true, defaultPath: isWin ? undefined : '/Volumes' }); if (p) { w.dest_root = p; draw(); } };
    if ($('#sched', d)) $('#sched', d).onchange = e => { read(); w.sched = e.target.value; draw(); };
  };
  function read() {
    const v = id => $('#' + id, d);
    if (v('base')) w.dest_base = v('base').value.trim();
    if (v('hours')) w.hours = +v('hours').value;
    if (v('time')?.value) [w.hour, w.minute] = v('time').value.split(':').map(Number);
    if (d.querySelector('[data-day]')) w.days = [...d.querySelectorAll('[data-day]:checked')].map(c => +c.dataset.day);
    if (v('catch')) w.catch_up = v('catch').checked;
    if (v('max')) w.max_gb = v('max').value;
    if (v('name')) w.name = v('name').value;
  }
  async function next() {
    read(); error = '';
    if (step === 0 && !w.folders.length) error = 'Add at least one folder.';
    if (step === 1 && !w.dest_root) error = 'Choose the folder on your NAS.';
    if (step === 1 && !error) {
      try { const t = await call('check_destination', { path: w.dest_root }); if (!t.ready) { await call('init_destination', { path: w.dest_root }); } } catch (e) { error = String(e); }
    }
    if (error) return draw();
    if (step < 4) { step++; return draw(); }
    const schedule = { manual: { kind: 'manual' }, every_hours: { kind: 'every_hours', hours: w.hours }, daily: { kind: 'daily', hour: w.hour, minute: w.minute }, weekly: { kind: 'weekly', days: w.days, hour: w.hour, minute: w.minute } }[w.sched];
    const job = { id: 0, name: w.name || 'Backup', dest_root: w.dest_root, schedule, paused: false, catch_up: w.catch_up, exclude_names: [], max_file_bytes: w.max_gb ? Math.round(+w.max_gb * 1e9) : null,
      folders: w.folders.map(f => ({ source: f, dest: `${w.dest_base}/${base(f)}` })) };
    try { const r = await call('save_job', { job }); if ($('#first', d).checked) await call('run_job', { job_id: r.job_id }); d.close(); render(); } catch (e) { error = String(e); draw(); }
  }
  draw(); d.showModal();
}

document.addEventListener('click', async e => {
  const t = e.target.closest('[data-run],[data-pause],[data-open],[data-back],[data-test],[data-log],[data-approve],[data-revoke],[data-go]'); if (!t) return;
  e.preventDefault(); const ds = t.dataset;
  if (ds.run) { await call('run_job', { job_id: +ds.run }); }
  else if (ds.pause) { await call(ds.p === 'true' ? 'resume_job' : 'pause_job', { job_id: +ds.pause }); }
  else if (ds.open) { detailId = +ds.open; }
  else if (ds.back !== undefined) { detailId = null; }
  else if (ds.test) {
    const b = t; const label = b.textContent; b.disabled = true; b.textContent = 'Testing…';
    try { const r = await call('test_connection', { job_id: +ds.test }); toast(r.ok ? 'The NAS is reachable.' : (r.error || 'The NAS could not be reached.'), !r.ok); }
    catch (e) { toast('Could not test the NAS: ' + (e && e.message || e), true); }
    finally { b.disabled = false; b.textContent = label; }
  }
  else if (ds.log) { const rows = await call('get_run_log', { run_id: +ds.log }); $('#log').innerHTML = `<h2>Run log</h2><div class="card mono">${rows.map(r => `${esc(r.kind)} ${esc(r.path)} ${esc(r.detail || '')}`).join('<br>') || 'Nothing logged.'}</div>`; return; }
  else if (ds.approve) { await call('approve_mcp_client', { identity: ds.approve }); }
  else if (ds.revoke) { await call('revoke_mcp_client', { identity: ds.revoke }); }
  else if (ds.go) { view = ds.go; }
  render();
});
document.querySelectorAll('nav a').forEach(a => a.addEventListener('click', e => { e.preventDefault(); view = a.dataset.v; detailId = null; render(); }));
render(); timer = setInterval(() => { if (!$('#dlg').open && !$('#log')?.innerHTML) render(); }, 3000);
