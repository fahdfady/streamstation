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

  // Scrubber: fraction (0..1) -> seconds into the movie, clamped.
  function offsetFromFraction(durationSecs, fraction) {
    if (!isFinite(durationSecs) || durationSecs <= 0) return 0;
    var f = isFinite(fraction) ? fraction : 0;
    if (f < 0) f = 0;
    if (f > 1) f = 1;
    return durationSecs * f;
  }

  global.AdminCore = {
    moveEntry: moveEntry,
    removeEntry: removeEntry,
    appendEntries: appendEntries,
    offsetFromFraction: offsetFromFraction
  };
})(typeof window !== "undefined" ? window : globalThis);
