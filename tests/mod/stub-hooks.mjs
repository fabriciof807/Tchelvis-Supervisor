// Resolves the Herdr Desktop runtime modules ("herdr", "gpui-kit", "gpui-base")
// to inert stubs so the mod's pure helpers can be imported under plain Node.
import { registerHooks } from "node:module";

const STUBS = {
  herdr: `
    const empty = () => [];
    export const agents = empty, terminals = empty, panes = empty, spaces = empty;
    export const focused = () => null, props = () => ({}), theme = () => ({});
    export const run = () => {}, exec = async () => ({ code: 0, stdout: "" });
    export const hc = async () => null, shared = () => null, setShared = () => {};
    export const setAgentOrder = () => {}, window = {}, layout = {}, keys = {};
  `,
  "gpui-kit": `export class View {}; export const div = () => ({}), svg = () => ({}), image = () => ({});`,
  "gpui-base": `
    const noop = () => {};
    const chain = () => new Proxy(() => {}, { get: () => chain, apply: () => chain() });
    const makeState = (opts = {}) => {
      let val = String(opts?.value ?? "");
      return {
        value: () => val,
        set_value: (v) => { val = String(v ?? ""); },
        on: noop,
        focus: noop,
        release: noop,
        set_auto_grow: noop,
        set_rows: noop,
        set_soft_wrap: noop,
        set_selected_range: noop,
        selected_range: () => ({ start: 0, end: 0 }),
        replace: noop,
      };
    };
    export const v_flex = () => ({}), h_flex = () => ({});
    export const InputState = { new: makeState };
    export const TextareaState = { new: makeState };
    export const Input = { new: chain };
    export const Textarea = { new: chain };
  `,
};

registerHooks({
  resolve(specifier, context, next) {
    if (specifier in STUBS) return { url: `stub:${specifier}`, shortCircuit: true };
    return next(specifier, context);
  },
  load(url, context, next) {
    if (url.startsWith("stub:")) {
      return { format: "module", source: STUBS[url.slice(5)], shortCircuit: true };
    }
    return next(url, context);
  },
});
