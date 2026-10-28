// The MCP server form (/admin/mcp-servers/new and /admin/mcp-servers/<name>/edit): shows "Allow
// insecure connections" only while the URL starts with `http:`. Without this script the form
// still works: the server shows the box when the URL it renders starts with `http:`, and ignores
// the box for any other URL.
(function () {
  'use strict';

  var url = document.getElementById('url');
  var insecure = document.getElementById('insecure-field');
  if (!url || !insecure) {
    return;
  }

  function update() {
    insecure.hidden = !/^http:/i.test(url.value.trim());
  }

  url.addEventListener('input', update);
  update();
})();
