// The sign-in island. A form cannot put a token in the Authorization header,
// and the daemon takes the token in the header on purpose (session.rs: a
// request that already carries the cookie has nothing to exchange, and a token
// in a form body is a token a cross-site page could post). So this presents
// it once, over the header, and reloads: the server then renders the app,
// because the cookie is what carries the session from here on.
//
// Nothing here stores the token anywhere. The field is cleared either way.
(function () {
  'use strict';
  var form = document.getElementById('login-form');
  if (!form) return;
  var token = document.getElementById('token');
  var error = document.getElementById('login-error');
  var button = document.getElementById('signin');

  function fail(text) {
    error.textContent = text;
    error.hidden = false;
  }

  form.addEventListener('submit', function (ev) {
    ev.preventDefault();
    var t = token.value.trim();
    token.value = '';
    error.hidden = true;
    if (!t) return;
    button.disabled = true;
    fetch('/session', {
      method: 'POST',
      headers: { Authorization: 'Bearer ' + t },
      credentials: 'same-origin',
    })
      .then(function (res) {
        if (res.status === 204) {
          window.location.reload();
          return;
        }
        fail('That credential was not accepted.');
      })
      .catch(function () {
        fail('Could not reach the daemon.');
      })
      .then(function () {
        button.disabled = false;
      });
  });
  token.focus();
})();
