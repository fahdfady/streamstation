(function (global) {
  "use strict";

  function moveEntry(list, from, to) {
    if (from < 0 || from >= list.length) return list.slice();
    to = Math.max(0, Math.min(list.length - 1, to));
    if (from === to) return list.slice();
    var copy = list.slice();
    var item = copy.splice(from, 1)[0];
    copy.splice(to, 0, item);
    return copy;
  }

  function removeEntry(list, index) {
    if (index < 0 || index >= list.length) return list.slice();
    return list.slice(0, index).concat(list.slice(index + 1));
  }

  function appendEntries(list, items) {
    var seen = {};
    var i;
    for (i = 0; i < list.length; i++) seen[list[i].path] = true;
    var added = [];
    for (i = 0; i < items.length; i++) {
      if (!seen[items[i].path]) {
        seen[items[i].path] = true;
        added.push(items[i]);
      }
    }
    return list.concat(added);
  }

  global.AdminCore = {
    moveEntry: moveEntry,
    removeEntry: removeEntry,
    appendEntries: appendEntries
  };
})(typeof window !== "undefined" ? window : globalThis);
