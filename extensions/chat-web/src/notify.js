// Agent-wide turn notifications. Loaded once by the dashboard shell so it keeps
// running on every dashboard tab, even when no chat is open.
(function () {
  if (window.__chatNotify) return;
  window.__chatNotify = true;
  const PREFIX = window.__dashPrefix || '';
  const UNREAD_KEY = 'dar-chat-unread', CLAIM_KEY = 'dar-chat-claim', MUTE_KEY = 'dar-notify-sound';
  const tabId = Math.random().toString(36).slice(2);

  const readUnread = () => { try { return JSON.parse(localStorage.getItem(UNREAD_KEY)) || {}; } catch { return {}; } };
  const viewing = slot => !!document.getElementById('chat-root') && !document.hidden
    && document.hasFocus?.() !== false && window.__chatWeb?.slot === slot;

  // Per-browser last-seen times: { created, seen: { <session id or slot id>: ms } }.
  // `created` is when this browser first ran the feature; older unknown sessions count as read.
  const SEEN_KEY = 'dar-chat-seen';
  const readSeen = () => {
    try { let s = JSON.parse(localStorage.getItem(SEEN_KEY)); if (s && s.seen) return s; } catch {}
    let fresh = { created: Date.now(), seen: {} };
    try { localStorage.setItem(SEEN_KEY, JSON.stringify(fresh)); } catch {}
    return fresh;
  };
  const markSeen = (keys, at = Date.now()) => {
    try {
      let store = readSeen();
      for (let k of keys) if (k) store.seen[k] = Math.max(store.seen[k] || 0, at);
      localStorage.setItem(SEEN_KEY, JSON.stringify(store));
    } catch {}
  };
  window.__chatSeen = { mark: markSeen };
  readSeen();

  const play = () => {
    try {
      if (localStorage.getItem(MUTE_KEY) === 'off') return;
      let audio = window.__chatNotifyAudio ||= new Audio(PREFIX + '/chat/notify.mp3');
      audio.volume = 0.6; audio.currentTime = 0;
      audio.play()?.catch(() => {}); // autoplay policy: ignored until the user interacted
    } catch {}
  };

  // One tab plays each turn: every tab writes a claim, then the last writer wins.
  const claimAndPlay = turn => {
    try {
      let current = localStorage.getItem(CLAIM_KEY) || '';
      if (current.startsWith(turn + '|')) return;
      let mine = `${turn}|${tabId}`;
      localStorage.setItem(CLAIM_KEY, mine);
      setTimeout(() => { if (localStorage.getItem(CLAIM_KEY) === mine) play(); }, 150);
    } catch { play(); }
  };

  const onTurnEnded = ended => {
    if (ended.kind !== 'finished') return;
    claimAndPlay(ended.turn);
    if (viewing(ended.slot)) { markSeen([ended.slot, ended.session]); return; }
    try {
      let unread = readUnread();
      unread[ended.slot] = true;
      if (ended.session) unread[ended.session] = true;
      localStorage.setItem(UNREAD_KEY, JSON.stringify(unread));
      window.__chatWeb?.refreshSidebar?.();
    } catch {}
  };

  // Catch-up for turns that ended while no tab listened. Silent: no sound.
  let catching = false;
  const catchUp = async () => {
    if (catching) return;
    catching = true;
    try {
      let all = await fetch('/chat/sessions').then(r => r.json());
      let store = readSeen(), unread = readUnread(), changed = false;
      for (let s of all) {
        if (s.archived || s.busy) continue;
        let keys = [s.id, ...(s.slots || [])];
        if ((s.slots || []).some(viewing)) { markSeen(keys, Math.max(Date.now(), s.modified_ms || 0)); continue; }
        let seen = Math.max(store.created, ...keys.map(k => store.seen[k] || 0));
        if ((s.modified_ms || 0) > seen && !unread[s.id]) { unread[s.id] = true; changed = true; }
      }
      if (changed) { localStorage.setItem(UNREAD_KEY, JSON.stringify(unread)); window.__chatWeb?.refreshSidebar?.(); }
    } catch {}
    catching = false;
  };

  window.__chatNotifyHandle = onTurnEnded;
  let es = new EventSource('/chat/events');
  es.onopen = catchUp;
  catchUp();
  es.onmessage = e => { try { onTurnEnded(JSON.parse(e.data)); } catch {} };
})();
