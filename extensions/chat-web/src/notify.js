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
    if (viewing(ended.slot)) return;
    try {
      let unread = readUnread();
      unread[ended.slot] = true;
      if (ended.session) unread[ended.session] = true;
      localStorage.setItem(UNREAD_KEY, JSON.stringify(unread));
      window.__chatWeb?.refreshSidebar?.();
    } catch {}
  };

  window.__chatNotifyHandle = onTurnEnded;
  let es = new EventSource('/chat/events');
  es.onmessage = e => { try { onTurnEnded(JSON.parse(e.data)); } catch {} };
})();
