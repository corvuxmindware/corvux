// Corvux iOS folder-picker plugin.
//
// Wraps `UIDocumentPickerViewController(forOpeningContentTypes: [.folder])`,
// creates a security-scoped bookmark for the picked folder, persists access
// via a per-process registry, and exposes resume / release entry points so
// the Rust side can re-acquire access on cold start.
//
// Important iOS semantics:
//   - Bookmarks must be created with `.withSecurityScope`.
//   - You MUST call `startAccessingSecurityScopedResource()` before any
//     `Data(contentsOf:)` / `FileManager` calls and matching
//     `stopAccessingSecurityScopedResource()` when done. The pair must
//     balance — extra stop calls are silently no-op'd, missing stops leak
//     access until process exit.
//   - Bookmarks can go stale (user renames / moves the folder via Files.app).
//     The resolver returns `isStale=true`; we re-mint and hand the new blob
//     back so callers persist the freshest one.

import Foundation
import SwiftRs
import Tauri
import UIKit
import UniformTypeIdentifiers

enum FolderPickerEvent {
  case selected(URL)
  case cancelled
  case error(String)
}

class FolderPickerDelegate: NSObject, UIDocumentPickerDelegate {
  weak var plugin: IosPickerPlugin?

  init(plugin: IosPickerPlugin) {
    self.plugin = plugin
  }

  func documentPicker(
    _ controller: UIDocumentPickerViewController, didPickDocumentsAt urls: [URL]
  ) {
    guard let url = urls.first else {
      plugin?.fireEvent(.cancelled)
      return
    }
    plugin?.fireEvent(.selected(url))
  }

  func documentPickerWasCancelled(_ controller: UIDocumentPickerViewController) {
    plugin?.fireEvent(.cancelled)
  }
}

class IosPickerPlugin: Plugin {
  // Tracks active security-scoped sessions keyed by URL.path. We hold the
  // URL object itself because `stopAccessingSecurityScopedResource` must be
  // called on the SAME instance that started the scope.
  private var activeSessions: [String: URL] = [:]
  private var pickerDelegate: FolderPickerDelegate!
  private var onPickerEvent: ((FolderPickerEvent) -> Void)?

  override init() {
    super.init()
    pickerDelegate = FolderPickerDelegate(plugin: self)
  }

  func fireEvent(_ event: FolderPickerEvent) {
    onPickerEvent?(event)
    onPickerEvent = nil
  }

  @objc public func pickFolder(_ invoke: Invoke) throws {
    onPickerEvent = { [weak self] event in
      guard let self = self else { return }
      switch event {
      case .cancelled:
        invoke.reject("user cancelled folder picker")
      case .error(let msg):
        invoke.reject(msg)
      case .selected(let url):
        self.handlePickedFolder(url: url, invoke: invoke)
      }
    }

    DispatchQueue.main.async { [weak self] in
      guard let self = self else { return }
      let picker: UIDocumentPickerViewController
      if #available(iOS 14, *) {
        picker = UIDocumentPickerViewController(forOpeningContentTypes: [UTType.folder])
      } else {
        picker = UIDocumentPickerViewController(
          documentTypes: ["public.folder"], in: .open)
      }
      picker.delegate = self.pickerDelegate
      picker.allowsMultipleSelection = false
      picker.modalPresentationStyle = .fullScreen
      self.manager.viewController?.present(picker, animated: true, completion: nil)
    }
  }

  private func handlePickedFolder(url: URL, invoke: Invoke) {
    // The picker hands us a URL with scoped access already started for
    // the duration of the callback. Promote it to a persisted bookmark
    // and explicitly start a long-lived session so subsequent
    // `std::fs` calls from Rust work.
    let started = url.startAccessingSecurityScopedResource()
    do {
      // iOS bookmarks for picker-returned URLs use empty options.
      // Apple's iOS doc-picker guidance: pass [] for create and resolve;
      // the security scope is implicit because the URL originated from
      // UIDocumentPickerViewController, and the OS tracks it via the
      // process-level sandbox extension.
      // (The `.withSecurityScope` flag is macOS-style and isn't honored
      // for iOS picker URLs.)
      let bookmarkData = try url.bookmarkData(
        options: [],
        includingResourceValuesForKeys: nil,
        relativeTo: nil)
      let b64 = bookmarkData.base64EncodedString()
      activeSessions[url.path] = url

      invoke.resolve([
        "path": url.path,
        "displayName": url.lastPathComponent,
        "bookmark": b64,
      ])
    } catch {
      if started {
        url.stopAccessingSecurityScopedResource()
      }
      invoke.reject("failed to create bookmark: \(error.localizedDescription)")
    }
  }

  @objc public func resumeAccess(_ invoke: Invoke) throws {
    struct Args: Decodable { let bookmark: String }
    let args = try invoke.parseArgs(Args.self)
    guard let data = Data(base64Encoded: args.bookmark) else {
      invoke.reject("bookmark is not valid base64")
      return
    }

    var isStale = false
    do {
      let url = try URL(
        resolvingBookmarkData: data,
        options: [],
        relativeTo: nil,
        bookmarkDataIsStale: &isStale)

      // If the bookmark went stale (user moved/renamed the folder),
      // re-mint a fresh one and hand it back so the caller persists it.
      var refreshedBookmark = args.bookmark
      if isStale {
        let started = url.startAccessingSecurityScopedResource()
        defer {
          if started { url.stopAccessingSecurityScopedResource() }
        }
        if let fresh = try? url.bookmarkData(
          options: [],
          includingResourceValuesForKeys: nil,
          relativeTo: nil)
        {
          refreshedBookmark = fresh.base64EncodedString()
        }
      }

      // Idempotent: if we already have an active session for this path,
      // drop the previous URL's scope before starting a new one so we
      // don't leak scope handles across vault re-opens.
      if let existing = activeSessions.removeValue(forKey: url.path) {
        existing.stopAccessingSecurityScopedResource()
      }

      if !url.startAccessingSecurityScopedResource() {
        invoke.reject(
          "unable to start scoped access — bookmark resolved but permission was denied")
        return
      }
      activeSessions[url.path] = url

      invoke.resolve([
        "path": url.path,
        "bookmark": refreshedBookmark,
        "stale": isStale,
      ])
    } catch {
      invoke.reject("failed to resolve bookmark: \(error.localizedDescription)")
    }
  }

  @objc public func releaseAccess(_ invoke: Invoke) throws {
    struct Args: Decodable { let path: String }
    let args = try invoke.parseArgs(Args.self)
    if let url = activeSessions.removeValue(forKey: args.path) {
      url.stopAccessingSecurityScopedResource()
    }
    invoke.resolve([:])
  }
}

@_cdecl("init_plugin_ios_picker")
func initPlugin() -> Plugin {
  return IosPickerPlugin()
}
