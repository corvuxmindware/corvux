/// <reference types="vite/client" />

// TypeScript 6 turned on `noUncheckedSideEffectImports` by default, which
// rejects `import "./foo.css"` without a module declaration. The
// `vite/client` reference above pulls in declarations for CSS, images,
// and the other asset types Vite handles, so side-effect imports type-check
// without us disabling the new check.
