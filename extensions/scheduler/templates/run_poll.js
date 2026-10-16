(function () {
  var drawer = document.getElementById('cron-run-drawer');
  var logs = drawer.querySelector('.cron-run-logs');
  var metadata = drawer.querySelector('.cron-run-metadata');
  var cursor = Number(drawer.dataset.cursor);
  var timer, request, stopped = false;
  function stop() {
    stopped = true;
    clearTimeout(timer);
    if (request) request.abort();
    observer.disconnect();
    window.removeEventListener('pagehide', stop);
  }
  var observer = new MutationObserver(function () {
    if (!drawer.isConnected) stop();
  });
  observer.observe(drawer.parentNode, { childList: true });
  window.addEventListener('pagehide', stop);
  async function poll() {
    if (stopped || !drawer.isConnected) return stop();
    request = new AbortController();
    var timeout = setTimeout(function () { request.abort(); }, 5000);
    try {
      var response = await fetch(drawer.dataset.logsUrl + '?since=' + cursor, { signal: request.signal });
      if (stopped || !drawer.isConnected) return;
      if (response.status === 404) {
        drawer.querySelector('.cron-run-status').textContent = 'quiet no-op (not retained)';
        return stop();
      }
      if (!response.ok) throw new Error('HTTP ' + response.status);
      var data = await response.json();
      if (stopped || !drawer.isConnected) return;
      var stick = logs.scrollHeight - logs.scrollTop - logs.clientHeight < 20;
      data.events.forEach(function (event) {
        if (event.id <= cursor) return;
        var empty = logs.querySelector('.empty');
        if (empty) empty.remove();
        logs.insertAdjacentHTML('beforeend', event.html);
        cursor = event.id;
      });
      drawer.dataset.cursor = cursor;
      metadata.innerHTML = data.metadata;
      htmx.process(metadata);
      drawer.querySelector('.cron-run-status').textContent = data.outcome;
      drawer.querySelector('.cron-run-notice').textContent = '';
      if (stick) logs.scrollTop = logs.scrollHeight;
      if (data.outcome !== 'running') stop();
    } catch (error) {
      if (!stopped) drawer.querySelector('.cron-run-notice').textContent = 'Connection interrupted; retrying...';
    } finally {
      clearTimeout(timeout);
      request = null;
      if (!stopped) timer = setTimeout(poll, 1900);
    }
  }
  logs.scrollTop = logs.scrollHeight;
  timer = setTimeout(poll, 1900);
})();
