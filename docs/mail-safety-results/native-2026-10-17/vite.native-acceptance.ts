import base from "/Users/arsalan/code/codex-worktrees/36f9/dakia-desktop/apps/desktop/vite.config.ts";

const harnessPath =
  "/@fs/private/tmp/dakia-native-mail-safety-final-20261017/native-acceptance-harness.ts";
const harnessPlugin = {
  name: "dakia-native-acceptance-harness",
  transformIndexHtml: {
    order: "pre" as const,
    handler() {
      return [
        {
          tag: "script",
          attrs: { type: "module", src: harnessPath },
          injectTo: "body" as const,
        },
      ];
    },
  },
};

export default {
  ...base,
  plugins: [...(base.plugins ?? []), harnessPlugin],
  server: {
    ...base.server,
    fs: {
      allow: [
        "/Users/arsalan/code/codex-worktrees/36f9/dakia-desktop",
        "/private/tmp/dakia-native-mail-safety-final-20261017",
      ],
    },
  },
};
