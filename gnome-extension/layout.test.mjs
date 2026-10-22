import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import {runInNewContext} from 'node:vm';

const source = readFileSync(new URL('./extension.js', import.meta.url), 'utf8');
const helper = source.match(/\r?\nfunction verticalBox\([^)]*\) \{\r?\n[\s\S]*?\r?\n\}\r?\n/);
assert.ok(helper, 'vertical layouts must handle both Shell property names');
assert.doesNotMatch(source.replace(helper[0], ''), /\b(?:orientation|vertical)\s*:/);

// Execute the actual helper against each API shape. Unsupported properties
// throw, as GJS does, and the caller's layout properties must survive.
for (const modern of [false, true]) {
    const result = runInNewContext(`${helper[0]}\nverticalBox({x_expand: true});`, {
        HAS_ORIENTATION: modern,
        Clutter: {Orientation: {VERTICAL: 1}},
        St: {BoxLayout: class {
            constructor(props) {
                assert.ok(!((modern ? 'vertical' : 'orientation') in props));
                Object.assign(this, props);
            }
        }},
    });
    assert.equal(result.x_expand, true);
    assert.equal(modern ? result.orientation : result.vertical, modern ? 1 : true);
}

// The top bar swaps its label for an icon when there is nothing to draw. That
// only holds if every panel write goes through the helper that makes the swap.
const panelWrites = source.match(/\b_label\.clutter_text\.set_markup\(/g) ?? [];
assert.equal(panelWrites.length, 1, 'top-bar markup must go through _setPanelMarkup');
assert.match(source, /\n {4}_setPanelMarkup\(markup\) \{\n[\s\S]*?this\._label\.clutter_text\.set_markup\(markup\);/);

// Every vendor switch off in prefs stores vendor='', and the panel must agree:
// no indicators at all, not a silent Anthropic fallback. The gschema default
// stays 'anthropic', so only an explicit choice produces the empty list.
const vendorsSrc = source.match(/\n {4}_vendors\(\) \{\n[\s\S]*?\n {4}\}/);
assert.ok(vendorsSrc, 'extension.js must keep a _vendors()');
assert.doesNotMatch(vendorsSrc[0], /'anthropic'/, 'an empty vendor list must not fall back to anthropic');
const Extension = runInNewContext(`(class { ${vendorsSrc[0]} })`, {});
const ext = new Extension();
// Cross-realm arrays fail deepStrictEqual prototype checks; re-home them.
const vendorsOf = s => {
    ext._settings = {get_string: () => s};
    return [...ext._vendors()];
};
assert.deepEqual(vendorsOf(''), []);
assert.deepEqual(vendorsOf('anthropic, openai ,anthropic'), ['anthropic', 'openai']);

// The brand mark is prepended in _init, so the empty-markup state must not
// assign the same gicon to _icon too — that drew two identical marks side by
// side. The icon slot is the generic stand-in for vendors with no mark file.
assert.doesNotMatch(source, /this\._icon\.gicon\s*=/,
    'the empty-bar state must not re-assign the prepended brand mark to _icon');
assert.match(source, /this\._icon\.icon_name = 'application-x-executable-symbolic'/,
    'the no-mark fallback icon stays');

// The tabbed menu supersedes provider submenus, but the three existing menu
// appearance settings remain live upgrade-compatible controls.
for (const key of ['menu-summary-style', 'menu-show-icons', 'menu-compact']) {
    assert.match(source, new RegExp(`get_(?:string|boolean)\\('${key}'\\)`),
        `${key} must still affect the tabbed menu`);
    assert.match(source, new RegExp(`'${key}'`),
        `${key} must trigger an immediate redraw`);
}
assert.match(source, /entries\.find\(e => e\.id === this\._tab\) \|\| entries\[0\]/,
    'the selected provider tab must survive report redraws and fall back safely');

const prefsSource = readFileSync(new URL('./prefs.js', import.meta.url), 'utf8');
assert.match(prefsSource, /vendorList = \[[^\]]*'supergrok'/s,
    'SuperGrok must be available in the top-bar provider switches');

console.log('GNOME layout compatibility tests passed');
