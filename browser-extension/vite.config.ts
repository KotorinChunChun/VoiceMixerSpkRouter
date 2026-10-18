import { fileURLToPath } from "node:url";
import { resolve } from "node:path";
import { defineConfig } from "vite";

const root = fileURLToPath(new URL(".", import.meta.url));

export default defineConfig({
  build: {
    outDir: "dist",
    emptyOutDir: true,
    rollupOptions: {
      input: {
        service_worker: resolve(root, "src/service-worker.ts"),
        "audio-worklet": resolve(root, "src/audio-worklet.ts"),
        offscreen: resolve(root, "src/offscreen.html"),
        options: resolve(root, "src/options.html"),
        popup: resolve(root, "src/popup.html"),
        sidepanel: resolve(root, "src/sidepanel.html")
      },
      output: { entryFileNames: "[name].js", chunkFileNames: "assets/[name]-[hash].js" }
    }
  }
});
