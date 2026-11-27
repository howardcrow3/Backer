// Integrity check: re-reads the copies on the NAS and compares them with the checksums saved when each file was
// backed up. Nothing on the NAS is ever changed by it. Draws the card on the job page and the result dialog.
// Used by renderDetail() in app.js: integrityUi.card(j.integrity, id). Wires its own buttons.
const integrityUi = (() => {
  const SCHEDULES = {
    off: { kind: 'manual' },
    weekly: { kind: 'weekly', days: [7], hour: 3, minute: 0 },
    monthly: { kind: 'monthly', day: 1, hour: 3, minute: 0 },
  };
  // Which option the saved schedule is. Anything set by other means (an assistant, an older build) shows as "other".
  const choiceOf = s => !s || s.kind === 'manual' ? 'off' : s.kind === 'weekly' && s.days.length === 1 && s.days[0] === 7 ? 'weekly' : s.kind === 'monthly' && s.day === 1 ? 'monthly' : 'other';
  const files = n => `${fmtN(n)} file${n === 1 ? '' : 's'}`;

  const pill = v => ({
    ok: '<span class="pill ok">All intact</span>',
    problems: '<span class="pill bad-p">Problems found</span>',
    failed: '<span class="pill bad-p">Could not finish</span>',
    cancelled: '<span class="pill warn">Stopped</span>',
    running: '<span class="pill run">Checking</span>',
  }[v.status] || '');

  const summaryLine = v => {
    const r = v.report;
    if (v.status === 'failed') return esc(v.message || 'The NAS could not be reached.');
    if (v.status === 'ok') return `${files(r.ok)} read back from the NAS and every one matches its checksum (${fmtBytes(r.bytes)}).`;
    if (v.status === 'cancelled') return `Stopped after ${fmtN(r.checked)} of ${fmtN(r.total)} files.`;
    const bits = [r.damaged_total && `${fmtN(r.damaged_total)} damaged`, r.missing_total && `${fmtN(r.missing_total)} missing`, r.unreadable_total && `${fmtN(r.unreadable_total)} could not be read`].filter(Boolean);
    return `${bits.join(', ')} of ${files(r.checked)} checked.`;
  };

  const card = (g, id) => {
    if (!g) return '';
    const last = g.last, running = g.running || (last && last.status === 'running');
    const choice = choiceOf(g.schedule);
    const opt = (v, t) => `<option value="${v}" ${choice === v ? 'selected' : ''}>${t}</option>`;
    const state = running
      ? `<span class="pill run">Checking</span> <span class="note" data-iprog>Starting…</span>`
      : last ? `${pill(last)} <span class="note">${esc(fmtTime(last.started))}${last.trigger === 'schedule' ? ' · scheduled' : ''}</span><div style="margin-top:4px">${summaryLine(last)}</div>`
        : `<span class="dim">Not checked yet.</span>`;
    return `<div class="card" data-integrity="${id}"><div class="row"><div class="sub" style="flex:1">Backup integrity</div>
        ${running ? `<button data-icancel>Stop</button>` : `${last && last.status !== 'running' ? `<button class="link" data-idetail="${id}">Details</button>` : ''}<button data-icheck="${id}" ${g.files ? '' : 'disabled'}>Check now</button>`}</div>
      <div>${state}</div>
      <div class="row" style="margin-top:10px"><label for="isched" style="margin:0">Check automatically</label>
        <select id="isched" data-ischedule="${id}" ${running ? 'disabled' : ''}>${opt('off', 'Never')}${opt('weekly', 'Every week')}${opt('monthly', 'Every month')}${choice === 'other' ? opt('other', 'Custom') : ''}</select>
        ${choice !== 'off' && g.next ? `<span class="note">Next: ${esc(fmtTime(g.next))}</span>` : ''}</div>
      <div class="sub" style="margin-top:8px">Reads every file on the NAS and compares it with the checksum saved at backup time, to catch damage the NAS did not report. It only reads: nothing on the NAS is changed or deleted. It can take a while for a large backup.</div></div>`;
  };

  const list = (items, total) => `<div class="card mono" style="max-height:140px;overflow:auto">${items.map(esc).join('<br>')}${total > items.length ? `<br>and ${fmtN(total - items.length)} more` : ''}</div>`;
  const detailHtml = v => {
    const r = v.report;
    const sec = (title, note, items, total) => total ? `<h2 style="font-size:14px;margin:14px 0 4px">${fmtN(total)} ${title}</h2><div class="sub">${note}</div>${list(items, total)}` : '';
    const head = { ok: 'Every file on the NAS is intact', problems: 'Some copies on the NAS have a problem', failed: 'The check could not finish', cancelled: 'The check was stopped' }[v.status] || 'Integrity check';
    return `<h1 style="font-size:18px">${head}</h1><p>${summaryLine(v)}</p>
      ${sec(`damaged cop${r.damaged_total === 1 ? 'y' : 'ies'}`, 'The file on the NAS is not the same as when it was backed up. Backer has not changed it. If the file on this computer is fine, delete the NAS copy yourself and run the backup again; the next backup copies it afresh.', r.damaged, r.damaged_total)}
      ${sec(`file${r.missing_total === 1 ? '' : 's'} missing from the NAS`, 'Backer copied these before, but they are no longer there. They are not copied again unless they change here, so restore them or copy them back if you still need them.', r.missing, r.missing_total)}
      ${sec('could not be read', 'The NAS did not answer for these. Run the check again later.', r.unreadable, r.unreadable_total)}
      <div class="sub" style="margin-top:8px">Checked ${fmtN(r.checked)} of ${fmtN(r.total)} files in ${(r.duration_ms / 1000).toFixed(1)} s. Nothing on the NAS was changed.</div>
      <div class="row" style="justify-content:flex-end;margin-top:12px"><button id="ic">Close</button></div>`;
  };

  const fail = e => toast('Could not do that: ' + (e && e.message || e), true);

  document.addEventListener('click', async e => {
    const b = e.target.closest('[data-icheck],[data-icancel],[data-idetail]'); if (!b) return;
    try {
      if (b.dataset.icheck) {
        const r = await call('start_verification', { job_id: +b.dataset.icheck });
        if (!r.started) toast('Another check is already running. Try again when it has finished.', true);
        render();
      } else if (b.dataset.icancel) {
        await call('cancel_verification'); toast('Stopping the check…'); render();
      } else {
        const v = (await call('list_verifications', { job_id: +b.dataset.idetail, limit: 1 }))[0]; if (!v) return;
        const d = $('#dlg'); d.innerHTML = detailHtml(v); d.showModal(); $('#ic', d).onclick = () => d.close();
      }
    } catch (err) { fail(err); }
  });
  document.addEventListener('change', async e => {
    const s = e.target.closest('[data-ischedule]'); if (!s || !SCHEDULES[s.value]) return;
    try { await call('set_verify_schedule', { job_id: +s.dataset.ischedule, schedule: SCHEDULES[s.value] }); toast(s.value === 'off' ? 'Automatic checks are off.' : 'Backer will check your backup ' + (s.value === 'weekly' ? 'every Sunday at 3 AM.' : 'on the 1st of each month at 3 AM.')); render(); }
    catch (err) { fail(err); }
  });
  // Live progress while a check runs; when it ends, redraw so the result shows.
  setInterval(async () => {
    const el = document.querySelector('[data-iprog]'); if (!el) return;
    try {
      const st = await call('verification_status');
      if (!st.running) return render();
      el.textContent = st.total ? `${fmtN(st.checked)} of ${fmtN(st.total)} files` : 'Starting…';
    } catch { /* the next tick tries again */ }
  }, 1000);

  return { card, detailHtml, summaryLine, choiceOf, SCHEDULES };
})();
