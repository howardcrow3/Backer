// NAS step of the job wizard: a folder the OS already mounted, or a direct SFTP, SMB or NFS
// connection with share and folder browsing (spec FR-2 and section 5). Passwords go straight to the
// agent, which keeps them in the Keychain; this page never stores them.
//
// Used by wizard() in app.js through nasStep: html(w) draws the step, bind(w, d, redraw) wires it.
const nasStep = (() => {
  const PROTOS = [
    ['mounted', 'Mounted folder'],
    ['smb', 'SMB'],
    ['sftp', 'SFTP'],
    ['nfs', 'NFS'],
  ];
  const HINT = {
    smb: 'Windows file sharing. On a Synology: Control Panel ▸ File Services ▸ SMB.',
    sftp: 'Secure file transfer over SSH. On a Synology: Control Panel ▸ File Services ▸ FTP ▸ SFTP.',
    nfs: 'For NAS folders shared with NFS permissions for this computer. On a Synology: Control Panel ▸ File Services ▸ NFS, then the shared folder’s NFS Permissions.',
  };
  const enc = s => encodeURIComponent(s).replace(/%2F/gi, '/');
  const encPart = s => encodeURIComponent(s);

  function init(w) {
    if (w.nas) return;
    w.nas = { proto: 'mounted', host: '', port: '', user: '', password: '', share: '', path: '/', shares: null, folders: null,
      hostKey: null, pinned: null, trusted: false, connected: false, busy: '', msg: '', newFolder: null };
  }

  // URL of the server, share and folder now shown. Share-less placeholders are only used to list shares.
  function url(n, path = n.path, share = n.share) {
    const host = n.host.includes(':') ? `[${n.host}]` : n.host;
    const port = n.port ? `:${+n.port}` : '';
    const p = path === '/' ? '' : enc(path);
    if (n.proto === 'sftp') return `sftp://${encPart(n.user)}@${host}${port}${enc(path)}`;
    if (n.proto === 'smb') return `smb://${encPart(n.user)}@${host}${port}/${encPart(share || '_')}${p}`;
    const exp = share || '/_';
    const full = path === '/' ? exp : (exp === '/' ? path : exp + path);
    return `nfs://${host}${port}${enc(full)}?export=${encPart(exp)}`;
  }

  const folderLabel = n => n.proto === 'smb' ? `${n.share}${n.path === '/' ? '' : n.path}` : n.proto === 'nfs' ? `${n.share}${n.path === '/' ? '' : n.path}` : n.path;

  function html(w) {
    init(w); const n = w.nas;
    const tabs = `<div class="row" role="radiogroup" aria-label="Connection">${PROTOS.map(([v, t]) => `<label style="margin:0"><input type="radio" name="proto" style="width:auto;height:auto" value="${v}" ${n.proto === v ? 'checked' : ''}> ${t}</label>`).join('')}</div>`;
    const sub = `<label>Subfolder on the NAS (optional)</label><input id="base" value="${esc(w.dest_base)}" placeholder="Leave blank to back up straight into the folder above"><div class="sub">Each folder you back up gets a folder of the same name inside the destination.</div>`;
    if (n.proto === 'mounted') {
      return `<h1>Where should the backup go?</h1>${tabs}<div class="sub" style="margin-top:8px">${isWin ? 'Connect to your NAS share in File Explorer first (for example map it to a drive letter, or open \\\\nas\\share), then choose its folder.' : 'Mount your NAS share in Finder first (Go ▸ Connect to Server), then choose its folder.'}</div><div class="row"><input id="root" class="mono" readonly value="${esc(w.dest_root)}" placeholder="${isWin ? 'Z:\\\\backups' : '/Volumes/backups'}"><button id="pick">Choose…</button></div>${sub}<div class="sub" style="margin-top:8px">New destinations are marked with a small file so the app never writes to an unmounted share.</div>`;
    }
    const pw = n.proto !== 'nfs';
    const form = `<div class="sub" style="margin-top:8px">${HINT[n.proto]}</div>
      <div class="row"><div style="flex:3"><label>NAS name or IP address</label><input id="nhost" value="${esc(n.host)}" placeholder="nas.local or 192.168.1.20" autocomplete="off" spellcheck="false"></div>
      <div style="flex:1"><label>Port</label><input id="nport" type="number" min="1" max="65535" value="${esc(n.port)}" placeholder="${{ sftp: 22, smb: 445, nfs: 'auto' }[n.proto]}"></div></div>
      ${pw ? `<div class="row"><div style="flex:1"><label>User name</label><input id="nuser" value="${esc(n.user)}" autocomplete="username" spellcheck="false"></div>
      <div style="flex:1"><label>Password</label><input id="npass" type="password" value="${esc(n.password)}" autocomplete="current-password" placeholder="${n.connected ? 'Saved' : ''}"></div></div>
      <div class="sub">A NAS user with access to the backup folder only is safest. The password is kept in the ${isWin ? 'Windows Credential Manager' : 'macOS Keychain'}.</div>` : ''}
      <div class="row"><button id="nconnect" ${n.busy ? 'disabled' : ''}>${n.busy === 'connect' ? 'Connecting…' : n.connected ? 'Reconnect' : 'Connect'}</button></div>`;
    let key = '';
    if (n.hostKey && !n.trusted) {
      key = `<div class="banner">${n.pinned ? `<b>This NAS’s identity has changed.</b> Backer remembered a different key for ${esc(n.host)}. Only continue if you know why (for example the NAS was reset).` : `First connection to ${esc(n.host)}. Check that this key fingerprint matches the one your NAS shows, then trust it.`}
        <div class="mono" style="margin:6px 0">${esc(n.hostKey)}</div><button id="ntrust">${n.pinned ? 'Trust the new key' : 'Trust this NAS'}</button></div>`;
    }
    let shares = '';
    if (n.connected && n.proto !== 'sftp') {
      const label = n.proto === 'smb' ? 'Shared folder' : 'Exported folder';
      shares = `<label>${label}</label>${n.shares && n.shares.length ? `<select id="nshare"><option value="">Choose…</option>${n.shares.map(s => `<option ${s === n.share ? 'selected' : ''}>${esc(s)}</option>`).join('')}</select>` : ''}
        <input id="nsharetxt" value="${esc(n.share)}" placeholder="${n.proto === 'smb' ? 'Type the share name, e.g. backups' : 'Type the exported path, e.g. /volume1/backups'}" ${n.shares && n.shares.length ? 'style="display:none"' : ''}>
        ${n.shares && !n.shares.length ? `<div class="sub">The NAS did not list its ${n.proto === 'smb' ? 'shares' : 'exports'}; type the name instead.</div>` : ''}`;
    }
    let browser = '';
    const ready = n.connected && (n.proto === 'sftp' || n.share) && n.folders;
    if (ready) {
      const crumbs = folderLabel(n);
      browser = `<label>Folder</label><div class="card" style="padding:8px 12px">
        <div class="row" style="justify-content:space-between"><span class="mono">${esc(crumbs)}</span><span>${n.path !== '/' ? '<button class="link" id="nup">Up</button>' : ''}<button class="link" id="nmk">New folder</button></span></div>
        ${n.newFolder !== null ? `<div class="row"><input id="nnew" value="${esc(n.newFolder)}" placeholder="Folder name"><button id="nnewok">Create</button><button class="link" id="nnewx">Cancel</button></div>` : ''}
        <div style="max-height:160px;overflow:auto">${n.folders.length ? n.folders.map(f => `<div><button class="link" style="text-transform:none;letter-spacing:0;font-size:14px" data-nf="${esc(f)}">${esc(f)}</button></div>`).join('') : '<div class="sub">No folders here.</div>'}</div></div>
        <div class="row"><button class="pri" id="nuse">Back up into ${esc(crumbs)}</button></div>`;
    }
    const chosen = w.dest_root && /^(sftp|smb|nfs):/.test(w.dest_root) ? `<div class="card">Destination: <span class="mono">${esc(w.dest_root)}</span></div>${sub}` : '';
    const msg = n.msg ? `<div class="${n.msgBad ? 'err' : 'sub'}">${esc(n.msg)}</div>` : '';
    return `<h1>Where should the backup go?</h1>${tabs}${form}${key}${msg}${shares}${browser}${chosen}`;
  }

  function readForm(w, d) {
    const n = w.nas, v = id => $('#' + id, d);
    if (v('nhost')) n.host = v('nhost').value.trim();
    if (v('nport')) n.port = v('nport').value.trim();
    if (v('nuser')) n.user = v('nuser').value.trim();
    if (v('npass')) n.password = v('npass').value;
    if (v('nsharetxt') && v('nsharetxt').style.display !== 'none') {
      let s = v('nsharetxt').value.trim();
      if (n.proto === 'nfs' && s && !s.startsWith('/')) s = '/' + s;
      if (s !== n.share) { n.share = s; n.path = '/'; n.folders = null; }
    }
  }

  function bind(w, d, redraw) {
    init(w); const n = w.nas;
    const fail = (e, what) => { n.msg = `${what}: ${e && e.message || e}`; n.msgBad = true; };
    const res = (r, what) => { if (r && r.ok === false) { n.msg = `${what}: ${r.error}`; n.msgBad = true; return false; } return true; };
    d.querySelectorAll('input[name=proto]').forEach(r => r.onchange = () => {
      readForm(w, d); n.proto = r.value; n.connected = false; n.shares = null; n.folders = null; n.share = ''; n.path = '/'; n.hostKey = null; n.msg = '';
      w.dest_root = '';
      redraw();
    });
    const browse = async () => {
      n.busy = 'browse';
      try { const r = await call('nas_browse', { url: url(n) }); if (res(r, 'Could not open the folder')) { n.folders = r.folders; n.msg = ''; } else n.folders = null; }
      catch (e) { fail(e, 'Could not open the folder'); }
      n.busy = '';
    };
    const connect = async () => {
      readForm(w, d); n.msg = ''; n.msgBad = false;
      if (!n.host) { n.msg = 'Enter the NAS name or IP address.'; n.msgBad = true; return redraw(); }
      if (n.proto !== 'nfs' && !n.user) { n.msg = 'Enter the user name.'; n.msgBad = true; return redraw(); }
      n.busy = 'connect'; n.connected = false; n.folders = null; n.shares = null; redraw();
      try {
        if (n.proto === 'sftp') {
          const p = await call('nas_probe', { url: url(n, '/') });
          if (!res(p, 'Could not reach the NAS')) { n.busy = ''; return redraw(); }
          n.hostKey = p.host_key; n.pinned = p.pinned; n.trusted = p.trusted;
          if (!n.trusted) { n.busy = ''; return redraw(); }
        }
        if (n.proto !== 'nfs' && n.password) { await call('nas_save_password', { url: url(n, '/'), password: n.password }); n.password = ''; }
        if (n.proto === 'sftp') {
          const t = await call('nas_browse', { url: url(n, '/'), path: '/' });
          if (!res(t, 'Could not connect')) { n.busy = ''; return redraw(); }
          n.connected = true; n.path = '/'; n.folders = t.folders;
        } else {
          const s = await call('nas_list_shares', { url: url(n, '/') });
          if (s.ok === false && (s.kind === 'credentials' || s.kind === 'unreachable')) { res(s, 'Could not connect'); n.busy = ''; return redraw(); }
          n.connected = true; n.shares = s.ok === false ? [] : s.shares;
        }
        n.msg = 'Connected.'; n.msgBad = false;
      } catch (e) { fail(e, 'Could not connect'); }
      n.busy = ''; redraw();
    };
    if ($('#nconnect', d)) $('#nconnect', d).onclick = connect;
    if ($('#ntrust', d)) $('#ntrust', d).onclick = async () => {
      try { await call('nas_trust_host_key', { url: url(n, '/'), fingerprint: n.hostKey }); n.trusted = true; await connect(); }
      catch (e) { fail(e, 'Could not trust the key'); redraw(); }
    };
    if ($('#nshare', d)) $('#nshare', d).onchange = async e => { n.share = e.target.value; n.path = '/'; n.folders = null; if (n.share) await browse(); redraw(); };
    if ($('#nsharetxt', d)) $('#nsharetxt', d).onchange = async () => { readForm(w, d); if (n.share) await browse(); redraw(); };
    d.querySelectorAll('[data-nf]').forEach(b => b.onclick = async () => { n.path = (n.path === '/' ? '' : n.path) + '/' + b.dataset.nf; await browse(); redraw(); });
    if ($('#nup', d)) $('#nup', d).onclick = async () => { n.path = n.path.replace(/\/[^/]*$/, '') || '/'; await browse(); redraw(); };
    if ($('#nmk', d)) $('#nmk', d).onclick = () => { n.newFolder = ''; redraw(); };
    if ($('#nnewx', d)) $('#nnewx', d).onclick = () => { n.newFolder = null; redraw(); };
    if ($('#nnewok', d)) $('#nnewok', d).onclick = async () => {
      const name = $('#nnew', d).value.trim(); if (!name) return;
      try { await call('nas_create_folder', { url: url(n), path: n.path, name }); n.newFolder = null; n.path = (n.path === '/' ? '' : n.path) + '/' + name; await browse(); }
      catch (e) { fail(e, 'Could not create the folder'); }
      redraw();
    };
    if ($('#nuse', d)) $('#nuse', d).onclick = async () => {
      n.busy = 'test'; n.msg = 'Testing…'; n.msgBad = false; redraw();
      try {
        const t = await call('nas_test', { url: url(n) });
        if (res(t, 'Test connection failed')) { w.dest_root = url(n); n.msg = 'Connection works and Backer can write here.'; n.msgBad = false; }
      } catch (e) { fail(e, 'Test connection failed'); }
      n.busy = ''; redraw();
    };
  }

  return { html, bind, readForm, init };
})();
