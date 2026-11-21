// Passkeys on two pages. WebAuthn can only be called from script, so this is the one first-party
// script a page needs to work, not a convenience; Docs/technical/web-ui.md says why. Without it the
// passkey parts of the pages stay hidden and the password form is the way in.
//
// - The login page (`#passkey-signin`): a conditional request starts on load, so the browser can
//   offer a discoverable passkey in the username field's autofill. "Sign in with a passkey" sends
//   the same request without `mediation`, so the browser opens its own dialog. A username asks the
//   server for that user's passkeys instead (a non-resident key needs it). A browser allows one
//   WebAuthn request at a time, so each of the others aborts the conditional one first.
// - The setup page (`#passkey-setup`): "Add a passkey" and "Add a non-resident key" each ask the
//   server for registration options, call the browser, and post the result.
//
// All data goes to the server as JSON. The server's options use base64url strings for the byte
// fields; the browser wants buffers.
(function () {
  'use strict';

  var supported = Boolean(window.PublicKeyCredential && navigator.credentials && window.isSecureContext);

  function decode(text) {
    var base64 = text.replace(/-/g, '+').replace(/_/g, '/');
    var binary = window.atob(base64 + '==='.slice((base64.length + 3) % 4));
    var bytes = new Uint8Array(binary.length);
    for (var i = 0; i < binary.length; i++) {
      bytes[i] = binary.charCodeAt(i);
    }
    return bytes.buffer;
  }

  function encode(buffer) {
    var bytes = new Uint8Array(buffer);
    var binary = '';
    for (var i = 0; i < bytes.length; i++) {
      binary += String.fromCharCode(bytes[i]);
    }
    return window.btoa(binary).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
  }

  // The server's JSON as the browser's options: every byte field a buffer.
  function descriptor(item) {
    var copy = Object.assign({}, item);
    copy.id = decode(item.id);
    return copy;
  }

  function creationOptions(options) {
    var copy = Object.assign({}, options);
    copy.challenge = decode(options.challenge);
    copy.user = Object.assign({}, options.user, { id: decode(options.user.id) });
    if (options.excludeCredentials) {
      copy.excludeCredentials = options.excludeCredentials.map(descriptor);
    }
    return copy;
  }

  function requestOptions(options) {
    var copy = Object.assign({}, options);
    copy.challenge = decode(options.challenge);
    copy.allowCredentials = (options.allowCredentials || []).map(descriptor);
    return copy;
  }

  // What the server reads: the fields it checks, and only those.
  function created(credential) {
    var response = credential.response;
    var extensions = credential.getClientExtensionResults ? credential.getClientExtensionResults() : {};
    var result = {
      id: credential.id,
      rawId: encode(credential.rawId),
      type: credential.type,
      response: {
        attestationObject: encode(response.attestationObject),
        clientDataJSON: encode(response.clientDataJSON),
        transports: response.getTransports ? response.getTransports() : []
      },
      clientExtensionResults: {}
    };
    if (extensions.credProps) {
      result.clientExtensionResults.credProps = extensions.credProps;
    }
    return result;
  }

  function asserted(credential) {
    var response = credential.response;
    return {
      id: credential.id,
      rawId: encode(credential.rawId),
      type: credential.type,
      response: {
        authenticatorData: encode(response.authenticatorData),
        clientDataJSON: encode(response.clientDataJSON),
        signature: encode(response.signature),
        userHandle: response.userHandle ? encode(response.userHandle) : null
      }
    };
  }

  function post(url, body, headers) {
    return window.fetch(url, {
      method: 'POST',
      credentials: 'same-origin',
      headers: Object.assign({ 'Content-Type': 'application/json' }, headers || {}),
      body: JSON.stringify(body)
    }).then(function (response) {
      return response.json().catch(function () { return {}; }).then(function (json) {
        return { ok: response.ok, json: json };
      });
    });
  }

  // A message for someone who cancelled or let the browser's dialog time out, which is no error.
  var CANCELLED = 'The passkey wasn\u2019t used. Try again when you\u2019re ready.';
  var FAILED = 'Something went wrong. Try again.';
  var ALREADY = 'This passkey is already registered. Pick another key or device.';

  function problem(error) {
    if (error && error.doozers) {
      return error.message;
    }
    if (error && error.name === 'InvalidStateError') {
      // The authenticator holds a credential the server listed in `excludeCredentials`.
      return ALREADY;
    }
    if (error && error.name === 'NotAllowedError') {
      return CANCELLED;
    }
    return FAILED;
  }

  // An error with a message to show as it is.
  function said(message) {
    var error = new Error(message);
    error.doozers = true;
    return error;
  }

  function reply(result) {
    if (!result.ok) {
      throw said(result.json.error || FAILED);
    }
    return result.json;
  }

  function show(box, message) {
    box.textContent = message;
    box.hidden = false;
  }

  // The password form is behind "Use a password instead" where passkeys are offered. Open it when
  // they can't be used here.
  function openPassword() {
    var details = document.querySelector('details.password-signin');
    if (details) {
      details.open = true;
    }
  }

  function initSignin(root) {
    var error = root.querySelector('[data-passkey-error]');
    var elsewhere = root.querySelector('[data-passkey-elsewhere]');
    var section = root.querySelector('[data-passkey-controls]');
    var form = root.querySelector('form');
    var username = form.elements.username;
    var discover = root.querySelector('[data-passkey-discover]');
    var nonce = root.getAttribute('data-nonce');
    var origin = root.getAttribute('data-origin');
    var urls = {
      discover: root.getAttribute('data-discover-url'),
      username: root.getAttribute('data-username-url'),
      finish: root.getAttribute('data-finish-url')
    };

    if (window.location.origin !== origin) {
      // A passkey belongs to one address, so it can't be used at this one, whatever the browser
      // can do. Say where to go.
      root.hidden = false;
      section.hidden = true;
      elsewhere.hidden = false;
      openPassword();
      return;
    }
    if (!supported) {
      openPassword();
      return;
    }
    root.hidden = false;

    // The server keeps a ceremony for six minutes, and a conditional request stays open as long
    // as the page does, so it is replaced before its ceremony runs out.
    var RESTART = 5 * 60 * 1000;
    // After a conditional request the server refused: the first wait, its cap, and how many
    // refusals in a row before the page stops retrying by itself.
    var BACKOFF = 3000;
    var BACKOFF_CAP = 30000;
    var MAX_FAILURES = 3;
    var failures = 0;
    var conditional = null;
    var timer = null;
    var busy = false;

    function abortConditional() {
      clearTimeout(timer);
      if (conditional) {
        conditional.abort();
        conditional = null;
      }
    }

    // Asks the server to start (`url`, with `extra` in the body), calls the browser, and posts the
    // assertion. `signal` and `mediation` are for the conditional request only.
    function run(url, extra, signal) {
      var started;
      return post(url, Object.assign({ nonce: nonce }, extra)).then(reply).then(function (json) {
        started = json;
        var options = { publicKey: requestOptions(json.options.publicKey) };
        if (signal) {
          options.mediation = 'conditional';
          options.signal = signal;
        }
        return navigator.credentials.get(options);
      }).then(function (credential) {
        return post(urls.finish, {
          nonce: nonce,
          ceremony: started.ceremony,
          credential: asserted(credential)
        }).then(reply).catch(function (failure) {
          // The browser answered, and the server didn't take it.
          failure.afterAnswer = true;
          throw failure;
        });
      }).then(function (json) {
        window.location.assign(json.redirect);
      });
    }

    function startConditional() {
      abortConditional();
      if (!window.PublicKeyCredential.isConditionalMediationAvailable) {
        return;
      }
      window.PublicKeyCredential.isConditionalMediationAvailable().then(function (available) {
        if (!available || busy || conditional) {
          return;
        }
        var controller = new AbortController();
        conditional = controller;
        timer = setTimeout(startConditional, RESTART);
        run(urls.discover, {}, controller.signal).catch(function (failure) {
          if (conditional !== controller) {
            // Aborted on purpose, for another request.
            return;
          }
          conditional = null;
          clearTimeout(timer);
          if (failure && failure.afterAnswer) {
            // The ceremony is spent, so the autofill needs a new one. A passkey the server keeps
            // refusing would otherwise be offered and refused again at once, over and over, so
            // the new one waits, longer each time, and after a few failures in a row the page
            // stops until the user acts.
            show(error, problem(failure));
            failures++;
            if (failures < MAX_FAILURES) {
              timer = setTimeout(startConditional, Math.min(BACKOFF * Math.pow(2, failures - 1), BACKOFF_CAP));
            }
          } else if (!(failure && failure.name === 'NotAllowedError')) {
            show(error, problem(failure));
          }
        });
      });
    }

    // The explicit requests: abort the conditional one, run this one, and start the conditional
    // one again if it didn't sign in (after one that did, the page is going away).
    function explicit(url, extra) {
      if (busy) {
        return;
      }
      busy = true;
      failures = 0;
      error.hidden = true;
      abortConditional();
      run(url, extra, null).catch(function (failure) {
        show(error, problem(failure));
        busy = false;
        startConditional();
      });
    }

    discover.addEventListener('click', function () {
      explicit(urls.discover, {});
    });
    form.addEventListener('submit', function (event) {
      event.preventDefault();
      var name = username.value.trim();
      if (name) {
        explicit(urls.username, { username: name });
      } else {
        explicit(urls.discover, {});
      }
    });
    startConditional();
  }

  function initSetup(root) {
    var error = root.querySelector('[data-passkey-error]');
    var controls = root.querySelector('[data-passkey-controls]');
    var unsupported = root.querySelector('[data-passkey-unsupported]');
    var elsewhere = root.querySelector('[data-passkey-elsewhere]');
    var name = root.querySelector('[data-passkey-name]');
    var csrf = root.getAttribute('data-csrf');
    var origin = root.getAttribute('data-origin');
    var urls = {
      start: root.getAttribute('data-start-url'),
      finish: root.getAttribute('data-finish-url')
    };

    if (window.location.origin !== origin) {
      // A passkey belongs to one address, so say where to open the page, whatever the browser can do.
      unsupported.hidden = true;
      elsewhere.hidden = false;
      return;
    }
    if (!supported) {
      unsupported.hidden = false;
      return;
    }
    unsupported.hidden = true;
    controls.hidden = false;

    var busy = false;
    root.querySelectorAll('[data-passkey-add]').forEach(function (button) {
      button.addEventListener('click', function () {
        if (busy) {
          return;
        }
        busy = true;
        error.hidden = true;
        var headers = { 'X-CSRF-Token': csrf };
        var started;
        post(urls.start, { kind: button.getAttribute('data-passkey-add') }, headers).then(reply).then(function (json) {
          started = json;
          return navigator.credentials.create({ publicKey: creationOptions(json.options.publicKey) });
        }).then(function (credential) {
          return post(urls.finish, {
            ceremony: started.ceremony,
            name: name.value,
            credential: created(credential)
          }, headers);
        }).then(reply).then(function (json) {
          window.location.assign(json.redirect);
        }).catch(function (failure) {
          show(error, problem(failure));
          busy = false;
        });
      });
    });
  }

  var signin = document.getElementById('passkey-signin');
  if (signin) {
    initSignin(signin);
  }
  var setup = document.getElementById('passkey-setup');
  if (setup) {
    initSetup(setup);
  }
})();
