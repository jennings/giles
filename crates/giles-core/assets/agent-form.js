// The new-agent form (/admin/agents/new): fills the Id field from the Name field as the owner
// types, until the owner edits the Id by hand. Clearing the Id makes it follow the Name again.
// Without this script the form still works: the Id stays an editable field, and the server derives
// it from the name when it is left empty.
//
// `derive` must stay equal to `agent_id_from_name` in src/agents/create.rs: lowercase, each space
// to a hyphen, everything but a-z, 0-9 and hyphen removed.
(function () {
  'use strict';

  var name = document.getElementById('name');
  var id = document.getElementById('id');
  if (!name || !id) {
    return;
  }

  function derive(text) {
    return text.toLowerCase().replace(/ /g, '-').replace(/[^a-z0-9-]/g, '');
  }

  // A form shown again after an error keeps the id that was used: follow only if it is the one
  // the name gives (or none).
  var following = id.value === '' || id.value === derive(name.value);

  name.addEventListener('input', function () {
    if (following) {
      id.value = derive(name.value);
    }
  });

  id.addEventListener('input', function () {
    following = id.value === '';
  });
})();
