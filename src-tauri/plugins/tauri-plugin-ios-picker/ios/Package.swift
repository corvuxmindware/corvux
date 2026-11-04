// swift-tools-version:5.3
import PackageDescription

let package = Package(
  name: "tauri-plugin-ios-picker",
  platforms: [
    .iOS(.v14)
  ],
  products: [
    .library(
      name: "tauri-plugin-ios-picker",
      type: .static,
      targets: ["tauri-plugin-ios-picker"])
  ],
  dependencies: [
    .package(name: "Tauri", path: "../.tauri/tauri-api")
  ],
  targets: [
    .target(
      name: "tauri-plugin-ios-picker",
      dependencies: [
        .byName(name: "Tauri")
      ],
      path: "Sources")
  ]
)
