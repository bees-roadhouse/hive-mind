// The live half of a thread. htmx renders and swaps every fragment; this
// island does the one thing a server-rendered page cannot: hold the
// conversation's SSE stream open and append text as it arrives.
//
// Every piece of text reaches the DOM through textContent. There is no markup
// path from a message body to the page, and the CSP the daemon sends makes
// that a property rather than a habit.
(function () {
  'use strict';
  var stream = null;
  var streamFor = '';

  function messages() {
    return document.getElementById('messages');
  }

  function lastSeq(box) {
    var max = 0;
    box.querySelectorAll('.msg[data-seq]').forEach(function (el) {
      var n = parseInt(el.getAttribute('data-seq'), 10);
      if (n > max) max = n;
    });
    return max;
  }

  function stickToBottom(box) {
    var stick = box.scrollTop + box.clientHeight >= box.scrollHeight - 40;
    if (stick) box.scrollTop = box.scrollHeight;
  }

  function liveBubble(box, seq, create) {
    var el = box.querySelector('.msg.live[data-request="' + seq + '"]');
    if (el || !create) return el;
    el = document.createElement('div');
    el.className = 'msg agent live';
    el.setAttribute('data-request', String(seq));
    var who = document.createElement('span');
    who.className = 'who thinking';
    who.textContent = 'agent is thinking';
    var text = document.createElement('span');
    text.className = 'text';
    el.appendChild(who);
    el.appendChild(text);
    box.appendChild(el);
    return el;
  }

  function close() {
    if (stream) stream.close();
    stream = null;
    streamFor = '';
  }

  function open(id) {
    if (streamFor === id && stream) return;
    close();
    streamFor = id;
    var es = new EventSource('/conversations/' + encodeURIComponent(id) + '/stream');
    stream = es;
    es.addEventListener('turn', function (ev) {
      var box = messages();
      if (!box || box.getAttribute('data-conversation') !== id) return;
      var t = JSON.parse(ev.data);
      if (t.state === 'done' || t.state === 'failed') {
        var live = liveBubble(box, t.request_seq, false);
        if (live) live.remove();
        htmx.ajax('GET', '/ui/conversations/' + encodeURIComponent(id) + '/messages?after=' + lastSeq(box), {
          target: '#messages',
          swap: 'beforeend',
        });
      } else {
        var el = liveBubble(box, t.request_seq, true);
        var who = el.querySelector('.who');
        who.textContent = t.state === 'claimed' ? 'agent is thinking' : 'waiting for an agent';
      }
    });
    es.addEventListener('run', function (ev) {
      var box = messages();
      if (!box || box.getAttribute('data-conversation') !== id) return;
      var f = JSON.parse(ev.data);
      if (!f.text) return;
      var el = liveBubble(box, f.request_seq, true);
      var who = el.querySelector('.who');
      who.textContent = 'agent';
      who.className = 'who';
      el.querySelector('.text').textContent += f.text;
      stickToBottom(box);
    });
    es.onerror = function () {
      // EventSource reconnects on its own. A revoked session shows up as a
      // stream that closes and never reopens; a cheap call turns that into the
      // sign-in page rather than a silent thread.
      if (es.readyState === EventSource.CLOSED) {
        fetch('/whoami', { credentials: 'same-origin' }).then(function (res) {
          if (res.status === 401) window.location.reload();
        });
      }
    };
  }

  function sync() {
    var box = messages();
    if (!box) {
      close();
      return;
    }
    var id = box.getAttribute('data-conversation') || '';
    if (id) open(id);
    stickToBottom(box);
    var body = document.getElementById('body');
    if (body && document.activeElement !== body && !body.value) body.focus();
  }

  document.body.addEventListener('htmx:afterSettle', sync);
  document.body.addEventListener('htmx:responseError', function (ev) {
    if (ev.detail && ev.detail.xhr && ev.detail.xhr.status === 401) {
      window.location.reload();
      return;
    }
    var notice = document.getElementById('notice');
    if (!notice) return;
    notice.textContent = 'That did not work (' + (ev.detail && ev.detail.xhr ? ev.detail.xhr.status : '?') + ').';
    notice.hidden = false;
    setTimeout(function () {
      notice.hidden = true;
    }, 4000);
  });
  document.body.addEventListener('keydown', function (ev) {
    if (ev.target && ev.target.id === 'body' && ev.key === 'Enter' && (ev.metaKey || ev.ctrlKey)) {
      ev.preventDefault();
      var form = ev.target.form;
      if (form) form.requestSubmit();
    }
  });
  sync();
})();
