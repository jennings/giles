// The agent forms (/admin/agents/new and /admin/agents/<agent>/edit). Without this script both
// still work: the Id stays an editable field, and the server derives it from the name when it is
// left empty; the environment notes are shown as the server rendered them.
//
// New agent form: fills the Id field from the Name field as the owner types, until the owner edits
// the Id by hand. Clearing the Id makes it follow the Name again.
//
// `derive` must stay equal to `agent_id_from_name` in src/agents/create.rs: lowercase, each space
// to a hyphen, everything but a-z, 0-9 and hyphen removed.
//
// Both forms: shows the notes under Environments as the owner ticks boxes. The environment tools
// are offered only to an agent with an environment, so one note says that an environment is ticked
// and none of its tools is, and the other that its tools are ticked and no environment is. `allows`
// must stay equal to the tool globs of src/agent_loop/glob.rs: `*` is any run of characters, `?`
// exactly one, everything else itself, over the whole name.
(function () {
  'use strict';

  var name = document.getElementById('name');
  var id = document.getElementById('id');
  if (name && id) {
    followName(name, id);
  }

  var noTools = document.getElementById('environment-note-no-tools');
  var noEnvironment = document.getElementById('environment-note-no-environment');
  if (noTools && noEnvironment) {
    showEnvironmentNotes(noTools, noEnvironment);
  }

  function derive(text) {
    return text.toLowerCase().replace(/ /g, '-').replace(/[^a-z0-9-]/g, '');
  }

  function followName(name, id) {
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
  }

  function allows(glob, tool) {
    var pattern = glob
      .replace(/[.+^${}()|[\]\\]/g, '\\$&')
      .replace(/\*/g, '.*')
      .replace(/\?/g, '.');
    return new RegExp('^' + pattern + '$').test(tool);
  }

  function showEnvironmentNotes(noTools, noEnvironment) {
    var form = noTools.closest('form');
    var other = document.getElementById('tools');
    var boxes = form.querySelectorAll('input[name^="environment:"]');
    var tools = form.querySelectorAll('input[data-environment-tool]');

    function update() {
      var ticked = Array.prototype.some.call(boxes, function (box) {
        return box.checked;
      });
      // A box that "Other tools" covers is ticked and disabled, so it counts; so does an entry
      // typed since the form was rendered.
      var globs = other ? other.value.split(/[,\s]+/).filter(Boolean) : [];
      var allowed = Array.prototype.some.call(tools, function (tool) {
        var value = tool.name.slice('tool:'.length);
        return tool.checked || globs.some(function (glob) {
          return allows(glob, value);
        });
      });
      noTools.hidden = !(ticked && !allowed);
      noEnvironment.hidden = !(!ticked && allowed);
    }

    form.addEventListener('change', update);
    if (other) {
      other.addEventListener('input', update);
    }
    update();
  }
})();
