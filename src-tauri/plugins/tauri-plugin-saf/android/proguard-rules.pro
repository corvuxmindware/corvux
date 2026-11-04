# Keep the Tauri plugin class, its annotated commands, and callbacks
# so R8 doesn't drop them when minify is on.
-keep class app.tauri.saf.** { *; }
