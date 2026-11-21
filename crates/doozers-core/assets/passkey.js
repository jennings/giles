// Adding a passkey on the setup page (`#passkey-setup`): "Add a passkey" and "Add a non-resident
// key" each ask the server for registration options, call the browser, and post the result.
// WebAuthn can only be called from script, so this is the one first-party script a page needs to
// work, not a convenience; Docs/technical/web-ui.md says why. Without it the buttons stay hidden.
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

  var setup = document.getElementById('passkey-setup');
  if (setup) {
    initSetup(setup);
  }
})();
