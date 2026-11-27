// "Send feedback" dialog. Stars (optional), a category, a message, an email (prefilled from the license) and a
// "Send anonymously" box that, when ticked, keeps the email from being sent at all. The app's Rust side
// signs and sends it to the High Turtle feedback service (src-tauri/src/feedback.rs). Uses $, esc, toast and T from app.js.
const FEEDBACK_CATEGORIES = [['feature', 'Idea'], ['issue', 'Problem'], ['praise', 'Praise'], ['other', 'Other']];
const feedbackPayload = (rating, category, message, email, anonymous = false) => ({ rating: rating || null, category, message: String(message || '').trim(), email: anonymous ? null : String(email || '').trim() || null, anonymous: !!anonymous });
const feedbackEmailOk = email => { const e = String(email || '').trim(); return !e || (e.length <= 254 && /^[^\s@"<>()[\],;:\\]+@(?:[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?\.)+[a-z]{2,63}$/i.test(e) && !/^\.|\.\.|\.@/.test(e)); };
const feedbackReady = (rating, message) => !!rating || String(message || '').trim().length > 0;

async function openFeedback() {
  const d = $('#dlg');
  let rating = 0, category = 'feature';
  const draft = { message: '', email: '', anonymous: false };
  try { const l = await call('license_status'); if (l && l.email) draft.email = l.email; } catch { /* no license: leave it blank */ }
  const draw = (err = '', busy = false) => {
    d.innerHTML = `<h1 style="font-size:18px">Send feedback</h1>
      <div class="sub">Tell us what to add, what went wrong or what you like. It goes straight to the people who make Backer.</div>
      <label id="fb-rl">How do you like Backer?</label>
      <div class="stars" role="radiogroup" aria-labelledby="fb-rl">${[1, 2, 3, 4, 5].map(n => `<button type="button" role="radio" aria-checked="${n === rating}" aria-label="${n} star${n > 1 ? 's' : ''}" data-star="${n}" class="${n <= rating ? 'on' : ''}">★</button>`).join('')}</div>
      <label>What's it about?</label>
      <div class="row fb-cats">${FEEDBACK_CATEGORIES.map(([v, t]) => `<label class="fb-cat"><input type="radio" name="fb-cat" value="${v}"${v === category ? ' checked' : ''}> ${t}</label>`).join('')}</div>
      <label for="fb-msg">Your feedback</label>
      <textarea id="fb-msg" rows="6" maxlength="5000" style="width:100%" placeholder="A feature you'd like, a problem you hit, or anything else">${esc(draft.message)}</textarea>
      <div class="sub" id="fb-count"></div>
      <label for="fb-email">Email (optional, so we can reply)</label>
      <input id="fb-email" type="email" style="width:100%" value="${esc(draft.email)}" autocomplete="email"${draft.anonymous ? ' disabled' : ''}>
      <label style="margin-top:8px"><input id="fb-anon" type="checkbox" style="width:auto;height:auto"${draft.anonymous ? ' checked' : ''}> Send anonymously (your email isn't sent)</label>
      <div class="err">${esc(err)}</div>
      <div class="row" style="justify-content:flex-end;margin-top:12px"><button id="fb-c">Cancel</button><button class="pri" id="fb-go"${busy ? ' disabled' : ''}>${busy ? 'Sending…' : 'Send'}</button></div>`;
    const msg = $('#fb-msg', d), email = $('#fb-email', d), anon = $('#fb-anon', d), go = $('#fb-go', d), count = $('#fb-count', d);
    const sync = () => {
      draft.message = msg.value; draft.email = email.value;
      if (!busy) go.disabled = !feedbackReady(rating, msg.value);
      count.textContent = msg.value.length > 4500 ? `${5000 - msg.value.length} characters left` : '';
    };
    msg.oninput = sync; email.oninput = sync; sync();
    anon.onchange = () => { sync(); draft.anonymous = anon.checked; draw(); };
    d.querySelectorAll('[data-star]').forEach(b => b.onclick = () => { rating = +b.dataset.star === rating ? 0 : +b.dataset.star; sync(); draw(); });
    d.querySelectorAll('input[name=fb-cat]').forEach(r => r.onchange = () => { category = r.value; });
    $('#fb-c', d).onclick = () => d.close();
    go.onclick = async () => {
      sync();
      if (!draft.anonymous && !feedbackEmailOk(draft.email)) { draw('That email address doesn\u2019t look right. Fix it, clear it, or tick Send anonymously.'); return; }
      draw('', true);
      try {
        await T.core.invoke('send_feedback', { feedback: feedbackPayload(rating, category, draft.message, draft.email, draft.anonymous) });
        d.close(); toast('Thanks! We read every one.');
      } catch (e) { draw(String((e && e.message) || e)); }
    };
  };
  draw(); d.showModal();
}

// Show the navigation entry only in builds that carry a feedback key.
T.core.invoke('feedback_available').then(ok => {
  const a = $('#fb-nav'); if (!a) return;
  a.hidden = !ok;
  a.onclick = e => { e.preventDefault(); e.stopImmediatePropagation(); openFeedback(); };
}).catch(() => {});
