// Talks to the background agent through the Tauri command `agent_call`. No business logic lives here.
const T = window.__TAURI__;
const call = (method, params = {}) => T.core.invoke('agent_call', { method, params });
const $ = (s, r = document) => r.querySelector(s);
const esc = s => String(s ?? '').replace(/[&<>"]/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]));
const fmtTime = s => s ? new Date(s * 1000).toLocaleString([], { month: 'short', day: 'numeric', hour: 'numeric', minute: '2-digit' }) : '—';
const fmtBytes = n => n > 1e9 ? (n / 1e9).toFixed(1) + ' GB' : n > 1e6 ? (n / 1e6).toFixed(1) + ' MB' : n > 1e3 ? (n / 1e3).toFixed(0) + ' KB' : n + ' B';
const fmtN = n => Number(n || 0).toLocaleString();
const fmtClock = s => s ? new Date(s * 1000).toLocaleTimeString([], { hour: 'numeric', minute: '2-digit', second: '2-digit' }) : '—';
const fmtDur = ms => { const t = Math.round((ms || 0) / 1000); if (!t) return ms ? '<1s' : '—'; const h = Math.floor(t / 3600), m = Math.floor(t % 3600 / 60), x = t % 60; return h ? `${h}h ${String(m).padStart(2, '0')}m` : m ? `${m}m ${String(x).padStart(2, '0')}s` : `${x}s`; };
const runMs = r => r.stats.duration_ms || (r.finished ? (r.finished - r.started) * 1000 : 0);
const triggerText = t => ({ schedule: 'Started by schedule', manual: 'Started by you' }[t] || '');
const isDest = m => /^destination not ready/i.test(m || '');
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
// Run screen state. Kept here so the 3-second refresh and "Load more" don't lose the filter or the rows already shown.
let runId = null, runLive = false, histLimit = 15, histFilter = 'all';
// Browse and restore: which job, which NAS folder, and what is ticked.
let browse = null;
const PAGE = 200;
let logState = { kind: 'all', search: '', rows: [], total: 0 };

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

// An assistant asked the user to set up a job. The user picks folders, NAS and times in the wizard.
async function showRequest() {
  const { request } = await call('new_job_request');
  let b = $('#req');
  if (request === null || request === undefined) { if (b) b.remove(); return; }
  if (!b) { b = document.createElement('div'); b.id = 'req'; b.className = 'banner'; document.body.prepend(b); }
  b.innerHTML = `<span>An assistant asked you to set up a new backup job${request ? ` called “${esc(request)}”` : ''}. You choose the folders, the NAS and the times.</span> <button class="pri" id="req-go">Set up…</button> <button id="req-no">Dismiss</button>`;
  $('#req-go').onclick = () => wizard(request);
  $('#req-no').onclick = async () => { await call('dismiss_new_job_request'); b.remove(); };
}

// Trial and license banner: shown when the trial is nearly over or has ended.
async function showLicense() {
  let l; try { l = await call('license_status'); } catch { return; }
  trialIndicator(l);
  let b = $('#lic');
  const show = l.state === 'expired' || (l.state === 'trial' && l.days_left <= 5);
  if (!show) { if (b) b.remove(); return; }
  if (!b) { b = document.createElement('div'); b.id = 'lic'; b.className = 'banner'; document.body.prepend(b); }
  b.innerHTML = `<span>${l.state === 'expired' ? 'Your free trial has ended, so backups are paused. Your backed-up files and history are untouched.' : `Your free trial ends in ${l.days_left} day${l.days_left === 1 ? '' : 's'}.`}</span> <button class="pri" data-buy>Buy Backer</button> <button data-go="settings">Enter a key</button>`;
}

// Always-visible trial indicator at the bottom of the left navigation. Hidden once a key is entered.
function trialIndicator(l) {
  const t = $('#trial'); if (!t) return;
  if (l.state === 'licensed') { t.hidden = true; return; }
  const ended = l.state === 'expired';
  const text = ended ? 'Trial ended' : `${l.days_left} day${l.days_left === 1 ? '' : 's'} left in trial`;
  t.className = 'trial' + (ended ? ' ended' : l.days_left <= 5 ? ' soon' : '');
  t.innerHTML = `<div class="t-days">${text}</div><button class="pri" data-buy>Buy now</button>`;
  t.hidden = false;
}

// App updates. Checked at most every 6 hours while the window is open; the user decides when to install.
let lastUpdateCheck = 0, pendingUpdate = null;
async function checkForUpdate(force = false) {
  if (!force && Date.now() - lastUpdateCheck < 6 * 3600 * 1000) return pendingUpdate;
  lastUpdateCheck = Date.now();
  try { pendingUpdate = await T.core.invoke('check_update'); } catch (e) { if (force) throw e; }
  return pendingUpdate;
}
async function showUpdate(force = false) {
  const u = await checkForUpdate(force);
  let b = $('#upd');
  if (!u) { if (b) b.remove(); return; }
  if (!b) { b = document.createElement('div'); b.id = 'upd'; b.className = 'banner'; document.body.prepend(b); }
  b.innerHTML = `<span>Backer ${esc(u.version)} is available (you have ${esc(u.current)}). Installing restarts Backer; a backup in progress stops and resumes at its next scheduled time.</span> <button class="pri" data-install>Install and restart</button>`;
}
async function appVersion() { try { return await T.app.getVersion(); } catch { return ''; } }

function licenseHtml(l) {
  const status = l.state === 'licensed' ? `<span class="pill ok">Licensed</span> Registered to ${esc(l.email)}. Thank you for buying Backer.`
    : l.state === 'trial' ? `<span class="pill warn">Free trial</span> ${l.days_left} day${l.days_left === 1 ? '' : 's'} left. Everything works during the trial.`
    : `<span class="pill bad-p">Trial ended</span> Backups are paused until you enter a license key. Viewing your history, Verify and restoring still work.`;
  return `<h2>License</h2><div class="card">${status}</div>
    ${l.state === 'licensed' ? '' : `<div class="sub">Paste the key from your purchase email.</div>
    <textarea id="lickey" rows="3" class="mono" style="width:100%" placeholder="BACKER-XXXXX-XXXXX-…"></textarea>
    <div class="row"><button class="pri" data-activate>Activate</button><button data-buy>Buy Backer</button></div>`}`;
}

async function render() {
  document.querySelectorAll('nav a').forEach(a => a.classList.toggle('on', a.dataset.v === view));
  try {
    $('#agent').textContent = 'Agent running';
    const jobs = await call('list_jobs');
    await showRequest();
    await showLicense();
    showUpdate().catch(() => {});
    const pending = jobs.reduce((n, j) => n + j.pending_deleted, 0);
    const b = $('#badge'); b.hidden = !pending; b.textContent = pending;
    if (view === 'jobs' && browse) return await renderBrowse();
    if (view === 'jobs') return runId ? await renderRun(runId) : detailId ? await renderDetail(detailId) : renderJobs(jobs, pending);
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

const stateOf = r => r.status === 'running' ? 'run' : r.status === 'ok' ? 'ok' : r.status === 'failed' ? 'er' : 'wn';
function resultPill(r) {
  const s = r.stats, bits = [];
  if (r.status === 'running') return '<span class="pill run">Running</span>';
  if (r.status === 'ok') return '<span class="pill ok">Synced</span>';
  if (r.status === 'failed') return `<span class="pill bad-p">Failed</span> <span class="note">${esc(isDest(r.message) ? 'NAS not reachable' : r.message || '')}</span>`;
  if (r.status === 'cancelled') return '<span class="pill warn">Cancelled</span> <span class="note">Stopped before it finished</span>';
  if (s.warnings) bits.push(`${fmtN(s.warnings)} couldn’t be read`);
  if (s.skipped) bits.push(`${fmtN(s.skipped)} skipped`);
  return `<span class="pill warn">Warnings</span> <span class="note">${esc(bits.join(' · '))}</span>`;
}
const needsAttention = r => r.status !== 'ok' && r.status !== 'running';

function sparkline(runs) {
  const last = runs.slice(0, 15).reverse(), max = Math.max(1, ...last.map(r => r.stats.copied));
  return `<div class="spark" role="img" aria-label="Files copied in each of the last ${last.length} runs">${last.map(r => `<i class="${stateOf(r)}" style="height:${Math.max(4, Math.round(r.stats.copied / max * 56))}px" title="${esc(fmtTime(r.started))}: ${fmtN(r.stats.copied)} copied"></i>`).join('')}</div>`;
}

async function renderDetail(id) {
  const [j, runs] = await Promise.all([call('get_job', { job_id: id }), call('list_runs', { job_id: id, limit: Math.max(histLimit, 15) })]);
  const shown = runs.filter(r => histFilter === 'all' || needsAttention(r)).slice(0, histLimit);
  const rr = shown.map(r => `<tr class="click" tabindex="0" data-openrun="${r.id}"><td><b>${fmtTime(r.started)}</b></td><td class="num dim">${fmtDur(runMs(r))}</td><td class="num">${fmtN(r.stats.copied)}</td><td class="num">${r.stats.bytes ? fmtBytes(r.stats.bytes) : '—'}</td><td>${resultPill(r)}</td><td class="chev" aria-hidden="true">›</td></tr>`).join('');
  const last = runs[0];
  $('#main').innerHTML = `<div class="sub"><a href="#" data-back>Jobs</a> / ${esc(j.job.name)}</div>
    <div class="row"><h1 style="flex:1">${esc(j.job.name)} ${statusPill(j)}</h1><button data-test="${id}">Test NAS</button><button data-verify="${id}">Verify backup</button><button data-browse="${id}">Browse and restore</button><button class="danger-o" data-delete="${id}">Delete job</button><button class="pri" data-run="${id}">Run now</button></div>
    <div class="card"><div class="sub">Folders</div>${j.job.folders.map(f => `<div class="mono">${esc(f.source)} → ${esc(f.dest)}</div>`).join('')}<div class="sub" style="margin-top:8px">Schedule: ${schedText(j.job.schedule)} · Next: ${fmtTime(j.next_run)}</div></div>
    ${last ? `<div class="card strip"><div><div class="lbl">Last backup</div><div class="big">${fmtTime(last.started)}</div><div>${resultPill(last)}</div></div><div><div class="lbl">Last ${Math.min(15, runs.length)} runs · taller means more copied</div>${sparkline(runs)}</div></div>` : ''}
    <div class="row" style="margin-top:24px"><h2 style="flex:1;margin:0">Run history</h2><span class="chips"><button class="chip ${histFilter === 'all' ? 'on' : ''}" data-hist="all">All</button><button class="chip ${histFilter === 'attention' ? 'on' : ''}" data-hist="attention">Needs attention</button></span></div>
    ${rr ? `<table class="hist" style="margin-top:8px"><thead><tr><th>Started</th><th class="num">Duration</th><th class="num">Copied</th><th class="num">Size</th><th>Result</th><th></th></tr></thead><tbody>${rr}</tbody></table>` : `<div class="card">${runs.length ? 'No runs needed attention.' : 'No runs yet.'}</div>`}
    ${runs.length >= histLimit ? '<div style="text-align:center;margin-top:12px"><button data-histmore>Show older runs</button></div>' : ''}`;
}

// ---- One run: summary first, then the file log
const KIND_LABEL = { new: 'New', updated: 'Updated', copied: 'Copied', skipped: 'Skipped', warning: 'Warning', error: 'Error', deleted_flagged: 'Flagged', run: 'Event' };
const KIND_CLASS = { new: 'new', updated: 'upd', copied: 'upd', skipped: 'warn', warning: 'warn', error: 'bad-p', deleted_flagged: 'info', run: 'info' };
const CHIPS = [['all', 'All'], ['new', 'New'], ['updated', 'Updated'], ['copied', 'Copied'], ['skipped', 'Skipped'], ['warning', 'Warnings'], ['error', 'Errors'], ['deleted_flagged', 'Flagged'], ['run', 'Events']];
const HINTS = {
  'Folder can’t be opened': isWin
    ? 'Windows won’t let Backer open this folder, so nothing inside it was backed up. It may be a protected system folder or belong to another user. To include it, right-click the folder, choose Properties, then Security, and allow your account to read it. Nothing on the NAS is flagged as deleted because of this, and the rest of the backup is not affected.'
    : 'macOS won’t let Backer open this folder, so nothing inside it was backed up. Open System Settings, Privacy & Security, Full Disk Access and turn on Backer, or fix the folder’s permissions. Nothing on the NAS is flagged as deleted because of this, and the rest of the backup is not affected.',
  'Shortcut or link, not copied': 'Backer does not follow shortcuts or links, so only the shortcut itself exists here and nothing was copied for it. Back up the folder it points to if you need those files.',
  'Permission denied': 'Backer can’t read them, so they weren’t backed up. Check their permissions on this computer.',
  'Larger than the size limit': 'They weren’t backed up because they are bigger than this job’s file size limit.',
  'File vanished during backup': 'They were removed or moved while Backer was copying. The next run will pick up the new location.',
  'Source folder is not available': 'Check that the drive or folder is connected, then run again. Nothing on the NAS was flagged as deleted.'
};
const tile = (label, val, sub, cls = '') => `<div class="card tile"><span class="lbl">${label}</span><span class="fig ${cls}">${val}</span>${sub ? `<span class="lbl">${sub}</span>` : ''}</div>`;

function issueHtml(g, jobId) {
  const err = g.kind === 'error', n = g.count, dest = err && isDest(g.cause);
  const why = dest ? g.cause.replace(/^destination not ready:?\s*/i, '') : '';
  const unit = g.cause === 'Folder can’t be opened' ? 'folder' : 'file';
  const where = unit === 'folder' && n === 1 && g.sample[0];
  const title = dest ? 'Can’t reach your NAS' : err ? esc(g.cause) : `${fmtN(n)} ${unit}${n === 1 ? '' : 's'} · ${where ? `<span class="mono">${esc(g.sample[0])}</span> · ` : ''}${esc(g.cause || (g.kind === 'skipped' ? 'Skipped' : 'Problem'))}`;
  const hint = dest ? `${why ? why.charAt(0).toUpperCase() + why.slice(1) + '. ' : ''}Check that the NAS is on and you are on the same network.` : (HINTS[g.cause] || '');
  const act = dest ? `<button data-test="${jobId}">Test NAS</button>` : '';
  const more = n > g.sample.length ? `<div class="lbl">and ${fmtN(n - g.sample.length)} more. <a href="#" data-chip="${g.kind}">Show them in the file log</a></div>` : '';
  const files = g.sample.filter(Boolean).length ? `<div class="samples">${g.sample.filter(Boolean).map(p => `<div class="mono">${esc(p)}</div>`).join('')}${more}</div>` : '';
  return `<details class="issue"${unit === 'folder' ? ' open' : ''}><summary><span class="ic ${err ? 'er' : 'wn'}">${err ? '!' : '▲'}</span><span class="grow"><b>${title}</b>${hint ? `<span class="lbl block">${esc(hint)}</span>` : ''}</span>${act}<span class="chev" aria-hidden="true">›</span></summary>${files}</details>`;
}

async function renderRun(id) {
  const rep = await call('get_run_summary', { run_id: id });
  const r = rep.run, s = r.stats, j = await call('get_job', { job_id: r.job_id });
  runLive = r.status === 'running';
  const nothing = !s.scanned && !s.copied && !s.unchanged;
  const sec = runMs(r) / 1000, issues = s.warnings + s.skipped + s.errors;
  let banner;
  if (runLive) banner = ['run', 'Running now', 'This page updates while the backup runs.'];
  else if (r.status === 'ok') banner = ['ok', 'Backed up with nothing to look at', `${fmtN(s.copied)} file${s.copied === 1 ? '' : 's'} copied, ${fmtN(s.unchanged)} already on your NAS. Nothing was removed from it.`];
  else if (r.status === 'warnings') banner = ['wn', `Finished with ${fmtN(issues)} thing${issues === 1 ? '' : 's'} to look at`, `Everything else was copied. ${fmtN(s.copied)} file${s.copied === 1 ? '' : 's'} are now on your NAS, and nothing was removed from it.`];
  else if (r.status === 'cancelled') banner = ['wn', 'Stopped before it finished', `${fmtN(s.copied)} file${s.copied === 1 ? '' : 's'} were copied and are safe on your NAS. Nothing was removed from it.`];
  else banner = ['er', isDest(r.message) ? 'Backup didn’t run: your NAS wasn’t reachable' : 'Backup didn’t finish', s.copied ? `${fmtN(s.copied)} files were copied before it stopped. Nothing was removed from your NAS.` : 'Nothing was copied and nothing was removed from your NAS. Backer will try again at the next scheduled time.'];
  const sub = [triggerText(s.trigger), runLive ? '' : `took ${fmtDur(runMs(r)) === '—' ? 'under a second' : fmtDur(runMs(r))}`, r.finished ? `finished ${fmtClock(r.finished)}` : ''].filter(Boolean).join(' · ');
  const dash = v => nothing ? '—' : v;
  const tiles = [
    tile('Copied', dash(fmtN(s.copied)), s.new_files || s.updated ? `${fmtN(s.new_files)} new · ${fmtN(s.updated)} updated` : ''),
    tile('Unchanged', dash(fmtN(s.unchanged))),
    tile('Skipped', dash(fmtN(s.skipped)), s.skipped ? 'not copied' : '', s.skipped ? 'wn' : ''),
    s.errors ? tile('Errors', fmtN(s.errors), 'stopped the run', 'er') : tile('Warnings', dash(fmtN(s.warnings)), s.warnings ? 'couldn’t be read' : '', s.warnings ? 'wn' : ''),
    tile('Excluded', dash(fmtN(s.excluded)), s.excluded ? 'left out by name' : ''),
    tile('Data sent', s.bytes ? fmtBytes(s.bytes) : '—', s.bytes && sec >= 1 ? `${fmtBytes(s.bytes / sec)}/s average` : '')
  ].join('');
  const flagged = s.newly_deleted ? `<div class="issue flat"><span class="ic info">i</span><span class="grow"><b>${fmtN(s.newly_deleted)} file${s.newly_deleted === 1 ? '' : 's'} flagged as deleted on this computer</b><span class="lbl block">Still on your NAS. Backer won’t remove them unless you say so.</span></span><button data-go="deleted">Review…</button></div>` : '';
  const attention = rep.issues.map(g => issueHtml(g, r.job_id)).join('') + flagged;
  const folders = rep.folders.length ? `<h2>By folder</h2><table class="num-table"><thead><tr><th>Folder</th><th class="num">Scanned</th><th class="num">Copied</th><th class="num">Size</th><th class="num">Issues</th><th class="num">Time</th></tr></thead><tbody>${rep.folders.map(f => `<tr><td class="mono">${esc(f.source)}</td><td class="num">${fmtN(f.scanned)}</td><td class="num">${fmtN(f.copied)}</td><td class="num">${f.bytes ? fmtBytes(f.bytes) : '—'}</td><td class="num ${f.issues ? 'wn-t' : 'dim'}">${fmtN(f.issues)}</td><td class="num dim">${fmtDur(f.duration_ms)}</td></tr>`).join('')}</tbody></table>` : '';
  const chips = CHIPS.filter(([k]) => k === 'all' || rep.counts[k]).map(([k, l]) => `<button class="chip ${logState.kind === k ? 'on' : ''}" data-chip="${k}">${l} <b>${fmtN(k === 'all' ? rep.log_rows : rep.counts[k])}</b></button>`).join('');
  const pruned = !rep.log_rows && !runLive && (s.copied || s.skipped || s.warnings || s.errors);
  $('#main').innerHTML = `<div class="sub"><a href="#" data-back-run>${esc(j.job.name)}</a></div>
    <div class="row"><h1 style="flex:1">Run on ${fmtTime(r.started)}</h1>${rep.log_rows ? '<button data-export>Export log</button>' : ''}<button data-browse="${r.job_id}">Restore files</button><button class="pri" data-run="${r.job_id}">Back up again</button></div>
    <div class="sub">${esc(sub)}</div>
    <div class="banner2 ${banner[0]}"><b>${esc(banner[1])}</b><span>${esc(banner[2])}</span></div>
    <div class="tiles">${tiles}</div>
    <h2>Needs attention</h2><div class="card list">${attention || '<div class="empty">Nothing needs attention.</div>'}</div>
    ${folders}
    <h2>File log</h2>
    ${pruned ? `<div class="card">The file-by-file log for this run was removed to save space. Backer keeps it for each job’s latest runs. The summary above is kept.</div>` : `<div class="card list">
      <div class="logbar"><span class="chips">${chips}</span><input id="q" type="search" placeholder="Search paths…" value="${esc(logState.search)}" aria-label="Search paths"></div>
      <div id="logrows"></div></div>`}`;
  if (!pruned) await loadLog(true);
}

function logRowHtml(r) {
  const size = r.size != null ? fmtBytes(r.size) : '';
  const main = r.path || r.detail || '';
  const right = r.path ? [size, r.detail].filter(Boolean).join(' · ') : size;
  return `<div class="lrow"><span class="mono dim t">${fmtClock(r.ts)}</span><span class="pill2 ${KIND_CLASS[r.kind] || 'info'}">${KIND_LABEL[r.kind] || esc(r.kind)}</span><span class="mono p" title="${esc(main)}">${esc(main)}</span><span class="mono dim d">${esc(right)}</span></div>`;
}

async function loadLog(reset) {
  const box = $('#logrows'); if (!box) return;
  if (reset) logState.rows = [];
  const q = { run_id: runId, kind: logState.kind, search: logState.search, offset: logState.rows.length, limit: PAGE };
  const page = await call('get_run_log', q);
  logState.rows = logState.rows.concat(page.rows); logState.total = page.total;
  const left = page.total - logState.rows.length;
  box.innerHTML = logState.rows.length
    ? `${logState.rows.map(logRowHtml).join('')}<div class="logfoot"><span class="lbl">Showing ${fmtN(logState.rows.length)} of ${fmtN(page.total)}</span>${left > 0 ? `<button data-more>Load ${fmtN(Math.min(PAGE, left))} more</button>` : ''}</div>`
    : '<div class="empty">No rows match.</div>';
}

const csvCell = v => { let t = String(v ?? ''); if (/^[=+\-@\t\r]/.test(t)) t = "'" + t; return /[",\n\r]/.test(t) ? '"' + t.replace(/"/g, '""') + '"' : t; };
async function exportLog() {
  const out = ['time,kind,path,detail,size_bytes'];
  for (let offset = 0; ; offset += 1000) {
    const page = await call('get_run_log', { run_id: runId, offset, limit: 1000 });
    for (const r of page.rows) out.push([r.ts ? new Date(r.ts * 1000).toISOString() : '', r.kind, r.path, r.detail, r.size ?? ''].map(csvCell).join(','));
    if (offset + 1000 >= page.total) break;
  }
  const saved = await T.core.invoke('save_text_file', { name: `backer-run-${runId}-log.csv`, text: out.join('\n') + '\n' });
  if (saved) toast('Log saved.');
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

// ---- Browse and restore (spec FR-31). Reads the NAS; asks before replacing anything on this computer.
async function renderBrowse() {
  const b = browse, j = await call('get_job', { job_id: b.jobId });
  let list, error = '';
  try { list = await call('browse_backup', { job_id: b.jobId, path: b.path }); } catch (e) { error = String((e && e.message) || e); }
  const parts = b.path ? b.path.split('/') : [];
  // The top level shows the job's folders, so the crumbs start at the backed-up folder, not the NAS root.
  const top = j.job.folders.find(f => b.path === f.dest || b.path.startsWith(f.dest + '/'));
  const crumbs = [`<a href="#" data-bpath="">All folders</a>`];
  if (top) {
    const skip = top.dest.split('/').length - 1;
    parts.forEach((p, i) => { if (i >= skip) crumbs.push(`<a href="#" data-bpath="${esc(parts.slice(0, i + 1).join('/'))}">${esc(p)}</a>`); });
  }
  const rows = (list ? list.entries : []).map(e => `<tr${e.is_dir ? ` class="click" data-bpath="${esc(e.path)}" tabindex="0"` : ''}>
      <td style="width:28px"><input type="checkbox" style="width:auto;height:auto" data-bpick="${esc(e.path)}" data-dir="${e.is_dir}" data-size="${e.size}" ${b.picked.has(e.path) ? 'checked' : ''} aria-label="Select ${esc(e.name)}"></td>
      <td>${e.is_dir ? '<span aria-hidden="true">▸ </span>' : ''}<b>${esc(e.name)}</b>${e.source ? `<div class="mono dim">from ${esc(e.source)}</div>` : ''}</td>
      <td class="num">${e.is_dir ? '—' : fmtBytes(e.size)}</td><td>${e.mtime ? fmtTime(e.mtime) : '—'}</td></tr>`).join('');
  const n = b.picked.size;
  $('#main').innerHTML = `<div class="sub"><a href="#" data-back>Jobs</a> / <a href="#" data-open="${b.jobId}">${esc(j.job.name)}</a> / Browse and restore</div>
    <div class="row"><h1 style="flex:1">Browse backup</h1></div>
    <div class="sub">What is on your NAS for this job. Tick files or folders, then choose Restore to put them back where they came from, or Restore to… to pick another folder. Nothing on the NAS is changed, and nothing on this computer is replaced unless you say so.</div>
    <div class="sub mono" style="margin-top:12px">${crumbs.join(' / ')}</div>
    ${error ? `<div class="banner">${esc(isDest(error) ? 'Your NAS isn’t reachable. Connect to it, then try again.' : error)}</div>`
      : rows ? `<table style="margin-top:8px"><thead><tr><th></th><th>Name</th><th class="num">Size</th><th>Modified</th></tr></thead><tbody>${rows}</tbody></table>` : '<div class="card">This folder is empty.</div>'}
    <div class="row" style="margin-top:12px"><span class="sub" style="flex:1;white-space:nowrap" id="bsel">${n ? `${n} selected` : 'Nothing selected'}</span>
      <button id="bclear" ${n ? '' : 'disabled'}>Clear</button><button id="bother" ${n ? '' : 'disabled'}>Restore to…</button><button class="pri" id="borig" ${n ? '' : 'disabled'}>Restore</button></div>`;
  document.querySelectorAll('#main [data-bpath]').forEach(el => el.addEventListener('click', ev => {
    if (ev.target.matches('input')) return;
    ev.preventDefault(); ev.stopPropagation(); browse.path = el.dataset.bpath; renderBrowse();
  }));
  document.querySelectorAll('#main [data-bpick]').forEach(el => el.addEventListener('change', () => {
    if (el.checked) b.picked.set(el.dataset.bpick, true); else b.picked.delete(el.dataset.bpick);
    renderBrowse();
  }));
  $('#bclear').onclick = () => { b.picked.clear(); renderBrowse(); };
  $('#borig').onclick = () => planRestore(null);
  $('#bother').onclick = async () => { const dir = await T.dialog.open({ directory: true }); if (dir) planRestore(dir); };
}

async function planRestore(target) {
  const b = browse, d = $('#dlg'), paths = [...b.picked.keys()];
  const where = target ? esc(target) : 'their original location';
  d.innerHTML = '<h1 style="font-size:18px">Getting ready…</h1><p>Listing the files to restore and checking what is already on this computer.</p>';
  d.showModal();
  let plan;
  try { plan = await call('plan_restore', { job_id: b.jobId, paths, target }); }
  catch (e) { d.innerHTML = `<h1 style="font-size:18px">Couldn’t restore</h1><p>${esc((e && e.message) || e)}</p><div class="row" style="justify-content:flex-end"><button id="c">Close</button></div>`; $('#c', d).onclick = () => d.close(); return; }
  const files = `${fmtN(plan.files)} file${plan.files === 1 ? '' : 's'} (${fmtBytes(plan.bytes)})`;
  if (!plan.files) { d.innerHTML = '<h1 style="font-size:18px">Nothing to restore</h1><p>The selected folders are empty on the NAS.</p><div class="row" style="justify-content:flex-end"><button id="c">Close</button></div>'; $('#c', d).onclick = () => d.close(); return; }
  const replaceable = plan.conflicts_total - plan.blocked_total;
  if (!plan.conflicts_total) {
    d.innerHTML = `<h1 style="font-size:18px">Restore ${files}?</h1><p>They will be copied from the NAS to ${where}. Nothing there is replaced.</p>
      <div class="row" style="justify-content:flex-end"><button id="c">Cancel</button><button class="pri" id="keep">Restore</button></div>`;
  } else {
    const row = c => `<tr><td class="mono">${esc(c.local)}</td><td>${c.blocked ? 'A folder is in the way' : `${fmtBytes(c.local_size)} · ${fmtTime(c.local_mtime)}`}</td><td>${fmtBytes(c.nas_size)} · ${fmtTime(c.nas_mtime)}</td></tr>`;
    d.innerHTML = `<h1 style="font-size:18px">${fmtN(plan.conflicts_total)} file${plan.conflicts_total === 1 ? ' is' : 's are'} already on this computer</h1>
      <p>Restoring ${files} to ${where}. These files already exist there. Backer replaces them only if you choose to; the rest are restored either way.</p>
      <div style="max-height:260px;overflow:auto"><table><thead><tr><th>On this computer</th><th>Yours now</th><th>Backup on NAS</th></tr></thead><tbody>${plan.conflicts.map(row).join('')}</tbody></table></div>
      ${plan.conflicts_total > plan.conflicts.length ? `<div class="sub">…and ${fmtN(plan.conflicts_total - plan.conflicts.length)} more.</div>` : ''}
      ${plan.blocked_total ? `<div class="sub">${fmtN(plan.blocked_total)} can’t be restored because a folder has the same name.</div>` : ''}
      <div class="row" style="justify-content:flex-end;margin-top:16px"><button id="c">Cancel</button><button id="keep">Keep my files</button>${replaceable ? `<button class="bad" id="repl">Replace ${fmtN(replaceable)}</button>` : ''}</div>`;
  }
  $('#c', d).onclick = () => d.close();
  $('#keep', d).onclick = () => runRestore(plan.plan_id, false);
  if ($('#repl', d)) $('#repl', d).onclick = () => runRestore(plan.plan_id, true);
}

async function runRestore(planId, replace) {
  const d = $('#dlg');
  try { await call('start_restore', { plan_id: planId, replace }); }
  catch (e) { d.innerHTML = `<h1 style="font-size:18px">Couldn’t restore</h1><p>${esc((e && e.message) || e)}</p><div class="row" style="justify-content:flex-end"><button id="c">Close</button></div>`; $('#c', d).onclick = () => d.close(); return; }
  for (;;) {
    let st; try { st = await call('restore_status'); } catch { st = null; }
    const r = st && st.report;
    if (!r) break;
    if (r.status === 'running') {
      const pct = r.bytes_total ? Math.floor(r.bytes_done / r.bytes_total * 100) : 0;
      d.innerHTML = `<h1 style="font-size:18px">Restoring…</h1><p>${fmtN(r.files_done)} of ${fmtN(r.files_total)} files · ${fmtBytes(r.bytes_done)} of ${fmtBytes(r.bytes_total)} (${pct}%)</p>
        <progress max="100" value="${pct}" style="width:100%"></progress><div class="row" style="justify-content:flex-end"><button id="stop">Stop</button></div>`;
      $('#stop', d).onclick = () => call('cancel_restore');
      await new Promise(res => setTimeout(res, 500));
      continue;
    }
    const title = r.status === 'done' ? (r.failed_total ? 'Restored, with problems' : 'Restored') : r.status === 'cancelled' ? 'Restore stopped' : 'Couldn’t restore';
    const bits = [`${fmtN(r.restored)} restored`, r.replaced ? `${fmtN(r.replaced)} replaced` : '', r.skipped_existing ? `${fmtN(r.skipped_existing)} kept as they were` : '', r.failed_total ? `${fmtN(r.failed_total)} couldn’t be restored` : ''].filter(Boolean).join(' · ');
    d.innerHTML = `<h1 style="font-size:18px">${title}</h1><p>${esc(r.message ? (isDest(r.message) ? 'Your NAS isn’t reachable. Connect to it, then try again.' : r.message) : bits)}</p>
      ${r.status === 'cancelled' ? '<p>Files already restored stay where they are.</p>' : ''}
      ${r.failed.length ? `<div class="card mono" style="max-height:200px;overflow:auto">${r.failed.map(f => `${esc(f.path)}: ${esc(f.error)}`).join('<br>')}</div>` : ''}
      <div class="row" style="justify-content:flex-end"><button class="pri" id="c">Done</button></div>`;
    $('#c', d).onclick = async () => { await call('dismiss_restore').catch(() => {}); d.close(); if (browse) { browse.picked.clear(); renderBrowse(); } };
    return;
  }
  d.close();
}

async function renderSettings() {
  const clients = await call('list_mcp_clients');
  const cfg = await T.core.invoke('mcp_config').catch(() => '');
  const lic = await call('license_status');
  $('#main').innerHTML = `<h1>Settings</h1>${licenseHtml(lic)}<h2>Updates</h2><div class="card">Backer ${esc(await appVersion())}</div><div class="row"><button data-checkupdate>Check for updates</button></div><h2>AI assistants (MCP)</h2>
    <div class="sub">Approved assistants can see jobs, run them, pause them, change schedules and delete a whole job (they must give its exact name), and ask you to set up a new job (you choose the folders, NAS and times). They cannot delete files from the NAS, or choose or change folders or destinations.</div>
    ${clients.length ? `<table><thead><tr><th>Program</th><th>Status</th><th>Last used</th><th></th></tr></thead><tbody>${clients.map(c => `<tr><td>${esc(c.client.name)}<div class="mono" style="color:var(--ink-muted)">${esc(c.identity.slice(0, 60))}</div></td><td>${c.client.approved ? '<span class="pill ok">Approved</span>' : '<span class="pill warn">Waiting for you</span>'}</td><td>${fmtTime(c.client.last_used)}</td>
      <td style="text-align:right">${c.client.approved ? '' : `<button class="pri" data-approve="${esc(c.identity)}">Allow</button> `}<button data-revoke="${esc(c.identity)}">${c.client.approved ? 'Revoke' : 'Deny'}</button></td></tr>`).join('')}</tbody></table>` : '<div class="card">No assistant has tried to connect.</div>'}
    <h2>Connect an assistant</h2>
    <div class="sub">Add this to your assistant's MCP settings (for Claude Desktop: Settings, Developer, Edit Config), then restart it. It will appear above, waiting for you to allow it.</div>
    <pre class="card mono" style="white-space:pre-wrap;user-select:all">${esc(cfg)}</pre>
    <button id="copycfg">Copy</button>`;
  const cb = $('#copycfg'); if (cb) cb.onclick = async () => { try { await navigator.clipboard.writeText(cfg); toast('Copied.'); } catch { toast('Select the text and copy it.', true); } };
}

// Result of "Verify backup": says plainly whether every file is on the NAS, and names any that are not.
function verifyHtml(r, id) {
  const list = (items, total) => `<div class="card mono" style="max-height:140px;overflow:auto">${items.map(esc).join('<br>')}${total > items.length ? `<br>and ${fmtN(total - items.length)} more` : ''}</div>`;
  const problems = r.missing_total + r.wrong_size_total + r.unreadable_total + r.folders_unavailable.length;
  const head = r.cancelled ? 'The check was stopped' : problems ? 'Some files are not on your NAS' : r.changed_total ? 'Everything is backed up, some files have changed since' : 'Every file is on your NAS';
  const sec = (title, note, items, total) => total ? `<h2 style="font-size:14px;margin:14px 0 4px">${fmtN(total)} ${title}</h2><div class="sub">${note}</div>${list(items, total)}` : '';
  const left = [r.links && `${fmtN(r.links)} shortcut${r.links === 1 ? '' : 's'}`, r.too_large && `${fmtN(r.too_large)} over the size limit`, r.excluded && `${fmtN(r.excluded)} left out by name`].filter(Boolean);
  return `<h1 style="font-size:18px">${head}</h1>
    <p>${fmtN(r.ok)} of ${fmtN(r.checked)} files checked are on the NAS with the right size.</p>
    ${r.folders_unavailable.length ? `<div class="banner">Not available, so not checked: ${r.folders_unavailable.map(esc).join(', ')}</div>` : ''}
    ${sec(`file${r.missing_total === 1 ? '' : 's'} missing from the NAS`, 'Run the backup to copy them. If they are still missing afterwards, open the run and look under warnings.', r.missing, r.missing_total)}
    ${sec('with the wrong size on the NAS', 'The NAS copy does not match this computer or what Backer saved. Run the backup to replace it.', r.wrong_size, r.wrong_size_total)}
    ${sec('could not be read here', 'Backer was not allowed to read these, so it could not check or back them up.', r.unreadable, r.unreadable_total)}
    ${sec('changed since the last backup', 'They are on the NAS, but the NAS copy is older. The next backup will update them.', r.changed, r.changed_total)}
    ${left.length ? `<div class="sub" style="margin-top:12px">Not part of the backup: ${left.join(', ')}.</div>` : ''}
    <div class="sub" style="margin-top:8px">This compares names and sizes, not file contents. Checked in ${(r.duration_ms / 1000).toFixed(1)} s.</div>
    <div class="row" style="justify-content:flex-end;margin-top:12px"><button id="vc">Close</button>${r.missing_total || r.wrong_size_total || r.changed_total ? '<button class="pri" id="vrun">Run backup now</button>' : ''}</div>`;
}

// ---- New job wizard (5 steps in one dialog)
function wizard(suggested = '') {
  const w = { name: suggested, folders: [], dest_root: '', dest_base: '', sched: 'daily', hour: 2, minute: 0, hours: 6, days: [1, 2, 3, 4, 5, 6, 7], catch_up: true, max_gb: '' };
  const destOf = f => w.dest_base ? `${w.dest_base}/${base(f)}` : base(f);
  let step = 0; const names = ['Folders', 'NAS', 'Schedule', 'Options', 'Review'];
  const d = $('#dlg'); let error = '';
  const draw = () => {
    const body = [
      `<h1>Which folders do you want to back up?</h1>${w.folders.map((f, i) => `<div class="card row"><span class="mono" style="flex:1">${esc(f)}</span><button class="link" data-rm="${i}">Remove</button></div>`).join('')}<button id="add">Add folder…</button>`,
      `<h1>Where should the backup go?</h1><div class="sub">${isWin ? 'Connect to your NAS share in File Explorer first (for example map it to a drive letter, or open \\\\nas\\share), then choose its folder.' : 'Mount your NAS share in Finder first (Go ▸ Connect to Server), then choose its folder.'}</div><div class="row"><input id="root" class="mono" readonly value="${esc(w.dest_root)}" placeholder="${isWin ? 'Z:\\\\backups' : '/Volumes/backups'}"><button id="pick">Choose…</button></div><label>Subfolder on the NAS (optional)</label><input id="base" value="${esc(w.dest_base)}" placeholder="Leave blank to back up straight into the folder above"><div class="sub">Each folder you back up gets a folder of the same name inside the destination.</div><div class="sub" style="margin-top:8px">New destinations are marked with a small file so the app never writes to an unmounted share.</div>`,
      `<h1>When should it run?</h1><label>Schedule</label><select id="sched">${[['manual', 'Only when I click Run now'], ['every_hours', 'Every few hours'], ['daily', 'Every day'], ['weekly', 'Certain days']].map(([v, t]) => `<option value="${v}" ${w.sched === v ? 'selected' : ''}>${t}</option>`).join('')}</select>
        ${w.sched === 'every_hours' ? `<label>Hours</label><input id="hours" type="number" min="1" max="168" value="${w.hours}">` : ''}
        ${['daily', 'weekly'].includes(w.sched) ? `<label>Time</label><input id="time" type="time" value="${String(w.hour).padStart(2, '0')}:${String(w.minute).padStart(2, '0')}">` : ''}
        ${w.sched === 'weekly' ? `<label>Days</label><div class="row">${['Mon', 'Tue', 'Wed', 'Thu', 'Fri', 'Sat', 'Sun'].map((n, i) => `<label style="margin:0"><input type="checkbox" style="width:auto;height:auto" data-day="${i + 1}" ${w.days.includes(i + 1) ? 'checked' : ''}> ${n}</label>`).join('')}</div>` : ''}
        <label style="margin-top:12px"><input id="catch" type="checkbox" style="width:auto;height:auto" ${w.catch_up ? 'checked' : ''}> Run at the next opportunity if the Mac was asleep</label>`,
      `<h1>A few options</h1><label>Skip files larger than (GB, blank for no limit)</label><input id="max" type="number" min="1" value="${esc(w.max_gb)}"><div class="card">Files you delete on this Mac stay on the NAS. The app lists them and nothing is removed until you confirm.</div>`,
      `<h1>Ready to start</h1><label>Name</label><input id="name" value="${esc(w.name || w.folders.map(f => base(f)).join(', '))}"><div class="card">${w.folders.map(f => `<div class="mono">${esc(f)} → ${esc(destOf(f))}</div>`).join('')}<div class="sub" style="margin-top:6px">Destination: ${esc(w.dest_root)} · ${w.sched === 'manual' ? 'Run now only' : w.sched}</div></div><label><input id="first" type="checkbox" style="width:auto;height:auto" checked> Start the first backup now</label>`,
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
    if (v('base')) w.dest_base = v('base').value.trim().replace(/^[\\/]+|[\\/]+$/g, '').replace(/\\/g, '/');
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
      folders: w.folders.map(f => ({ source: f, dest: destOf(f) })) };
    try { const r = await call('save_job', { job }); if ($('#first', d).checked) await call('run_job', { job_id: r.job_id }); d.close(); render(); } catch (e) { error = String(e); draw(); }
  }
  draw(); d.showModal();
}

document.addEventListener('click', async e => {
  const t = e.target.closest('[data-run],[data-pause],[data-delete],[data-open],[data-back],[data-test],[data-verify],[data-openrun],[data-back-run],[data-chip],[data-more],[data-export],[data-hist],[data-histmore],[data-approve],[data-revoke],[data-go],[data-activate],[data-buy],[data-install],[data-checkupdate],[data-browse]'); if (!t) return;
  e.preventDefault(); const ds = t.dataset;
  if (ds.run) { await call('run_job', { job_id: +ds.run }); }
  else if (ds.pause) { await call(ds.p === 'true' ? 'resume_job' : 'pause_job', { job_id: +ds.pause }); }
  else if (ds.open) { detailId = +ds.open; histLimit = 15; histFilter = 'all'; browse = null; }
  else if (ds.openrun) { runId = +ds.openrun; logState = { kind: 'all', search: '', rows: [], total: 0 }; }
  else if (ds.backRun !== undefined) { runId = null; }
  else if (ds.chip) { logState.kind = ds.chip; await loadLogOrRender(); return; }
  else if (ds.more !== undefined) { await loadLog(false); return; }
  else if (ds.export !== undefined) { try { await exportLog(); } catch (e) { toast('Could not save the log: ' + (e && e.message || e), true); } return; }
  else if (ds.hist) { histFilter = ds.hist; }
  else if (ds.histmore !== undefined) { histLimit += 15; }
  else if (ds.back !== undefined) { detailId = null; runId = null; browse = null; }
  else if (ds.delete) {
    const d = $('#dlg'), id = +ds.delete;
    const j = await call('get_job', { job_id: id });
    d.innerHTML = `<h1 style="font-size:18px">Delete “${esc(j.job.name)}”?</h1>
      <p>This removes the job and its history from Backer. It will not run again.</p>
      <p><b>Nothing is deleted from your NAS or your Mac.</b> Files already backed up stay on the NAS where they are.</p>
      <div class="row" style="justify-content:flex-end"><button id="c">Cancel</button><button class="bad" id="go">Delete job</button></div>`;
    d.showModal(); $('#c', d).onclick = () => d.close();
    $('#go', d).onclick = async () => {
      try { await call('delete_job', { job_id: id }); d.close(); view = 'jobs'; detailId = null; render(); toast('Job deleted. Files on the NAS were left alone.'); }
      catch (e) { d.close(); toast('Could not delete the job: ' + (e && e.message || e), true); }
    };
  }
  else if (ds.verify) {
    const id = +ds.verify, d = $('#dlg');
    d.innerHTML = '<h1 style="font-size:18px">Checking your backup…</h1><p>Comparing every file on this computer with the NAS. This can take a few minutes for a large backup. Nothing is copied or changed.</p>';
    d.showModal();
    try { d.innerHTML = verifyHtml(await call('verify_job', { job_id: id }), id); }
    catch (err) { d.innerHTML = `<h1 style="font-size:18px">Couldn’t check the backup</h1><p>${esc(err && err.message || err)}</p><div class="row" style="justify-content:flex-end"><button id="vc">Close</button></div>`; }
    $('#vc', d).onclick = () => d.close();
    const vr = $('#vrun', d); if (vr) vr.onclick = async () => { d.close(); await call('run_job', { job_id: id }); render(); };
    return;
  }
  else if (ds.test) {
    const b = t; const label = b.textContent; b.disabled = true; b.textContent = 'Testing…';
    try { const r = await call('test_connection', { job_id: +ds.test }); toast(r.ok ? 'The NAS is reachable.' : (r.error || 'The NAS could not be reached.'), !r.ok); }
    catch (e) { toast('Could not test the NAS: ' + (e && e.message || e), true); }
    finally { b.disabled = false; b.textContent = label; }
  }
  else if (ds.install !== undefined) {
    t.disabled = true; t.textContent = 'Installing…';
    try { await T.core.invoke('install_update'); } catch (e) { t.disabled = false; t.textContent = 'Install and restart'; toast(String((e && e.message) || e), true); }
    return;
  } else if (ds.checkupdate !== undefined) {
    t.disabled = true; t.textContent = 'Checking…';
    try { const u = await checkForUpdate(true); await showUpdate(); toast(u ? `Backer ${u.version} is available.` : 'Backer is up to date.'); }
    catch (e) { toast(String((e && e.message) || e), true); }
    t.disabled = false; t.textContent = 'Check for updates'; return;
  } else if (ds.buy !== undefined) { try { await T.core.invoke('open_purchase_page'); } catch (e) { toast('Could not open the store page: ' + (e && e.message || e), true); } return; }
  else if (ds.activate !== undefined) {
    const key = ($('#lickey') || {}).value || '';
    try { await call('set_license_key', { key }); toast('License activated. Thank you!'); }
    catch (e) { toast(e && e.message || String(e), true); }
  }
  else if (ds.approve) { await call('approve_mcp_client', { identity: ds.approve }); }
  else if (ds.revoke) { await call('revoke_mcp_client', { identity: ds.revoke }); }
  else if (ds.browse) { browse = { jobId: +ds.browse, path: '', picked: new Map() }; }
  else if (ds.go) { view = ds.go; runId = null; detailId = null; browse = null; }
  render();
});
// A chip click on the run page redraws the chips and rows without losing the page.
async function loadLogOrRender() { await renderRun(runId); }
document.addEventListener('keydown', e => { if (e.key === 'Enter' && e.target.matches('tr.click')) e.target.click(); });
let searchTimer = null;
document.addEventListener('input', e => {
  if (e.target.id !== 'q') return;
  clearTimeout(searchTimer);
  searchTimer = setTimeout(() => { logState.search = e.target.value.trim(); loadLog(true); }, 250);
});
document.querySelectorAll('nav a').forEach(a => a.addEventListener('click', e => { e.preventDefault(); view = a.dataset.v || 'jobs'; detailId = null; runId = null; browse = null; render(); }));
render(); timer = setInterval(() => { if (!$('#dlg').open && !browse && (!runId || runLive) && document.activeElement?.id !== 'q') render(); }, 3000);
