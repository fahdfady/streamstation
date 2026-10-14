// Tests for the /admin page's pure list-manipulation logic (web/admin-core.js),
// which drives drag-and-drop, the up/down buttons and "add from library".
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const code = readFileSync(join(root, "web", "admin-core.js"), "utf8");
(0, eval)(code);
const { moveEntry, removeEntry, appendEntries } = globalThis.AdminCore;

let failures = 0;
function check(cond, message) {
  if (cond) {
    console.log(`ok - ${message}`);
  } else {
    failures++;
    console.error(`FAIL - ${message}`);
  }
}

const row = (name) => ({ path: "/lib/" + name, title: name, reachable: true });
const names = (list) => list.map((e) => e.title).join(",");

const base = [row("a"), row("b"), row("c"), row("d")];

check(names(moveEntry(base, 0, 2)) === "b,c,a,d", "move down two slots");
check(names(moveEntry(base, 3, 0)) === "d,a,b,c", "move to the top");
check(names(moveEntry(base, 1, 1)) === "a,b,c,d", "move onto itself is a no-op");
check(names(moveEntry(base, 0, 99)) === "b,c,d,a", "move past the end clamps to last");
check(names(moveEntry(base, -1, 0)) === "a,b,c,d", "invalid source is a no-op");
check(names(base) === "a,b,c,d", "moveEntry does not mutate the input");

check(names(removeEntry(base, 0)) === "b,c,d", "remove first");
check(names(removeEntry(base, 3)) === "a,b,c", "remove last");
check(names(removeEntry(base, 7)) === "a,b,c,d", "remove out of range is a no-op");
check(names(base) === "a,b,c,d", "removeEntry does not mutate the input");

const added = appendEntries(base, [row("e"), row("a"), row("f")]);
check(names(added) === "a,b,c,d,e,f", "append adds only new paths, no duplicates");
check(names(base) === "a,b,c,d", "appendEntries does not mutate the input");

process.exitCode = failures > 0 ? 1 : 0;
