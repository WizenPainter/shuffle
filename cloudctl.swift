// cloudctl — download (materialize) or evict (free up space) a cloud file, or
// nudge a cloud folder to sync.
//
//   cloudctl download <path>   # pull an online-only file down to disk
//   cloudctl evict    <path>   # remove the local copy, keep it online-only
//   cloudctl sync     <dir>    # start the provider's sync app if it's not
//                              # running, and re-enumerate the folder
//   cloudctl status   <dir>    # "busy N": children still downloading/uploading
//
// Handles both iCloud Drive (Foundation ubiquitous APIs) and third-party File
// Provider stores (Dropbox, Google Drive, OneDrive, … via NSFileProviderManager
// / coordinated reads). Shuffle spawns this as a subprocess. Prints "ok" on
// success (`sync` prints "launched <App>" when it had to start the sync app);
// a message to stderr and a non-zero exit on failure.
import AppKit
import Foundation
import FileProvider

func fail(_ msg: String) -> Never {
    FileHandle.standardError.write((msg + "\n").data(using: .utf8)!)
    exit(1)
}

let args = CommandLine.arguments
guard args.count >= 3, ["download", "evict", "sync", "status"].contains(args[1]) else {
    fail("usage: cloudctl <download|evict|sync|status> <path>")
}
let action = args[1]
let url = URL(fileURLWithPath: args[2])
let fm = FileManager.default

// iCloud Drive lives under ~/Library/Mobile Documents; everything else that's a
// placeholder is a third-party File Provider store.
let isICloud = url.path.contains("/Library/Mobile Documents/")

/// The sync app behind a `~/Library/CloudStorage/<Provider>-<account>` store,
/// as (display name, candidate bundle ids).
func providerApp(for url: URL) -> (String, [String])? {
    let comps = url.pathComponents
    guard let i = comps.firstIndex(of: "CloudStorage"), i + 1 < comps.count else { return nil }
    let base = comps[i + 1].split(separator: "-").first.map(String.init) ?? comps[i + 1]
    switch base {
    case "Dropbox": return ("Dropbox", ["com.getdropbox.dropbox"])
    case "OneDrive": return ("OneDrive", ["com.microsoft.OneDrive-mac", "com.microsoft.OneDrive"])
    case "GoogleDrive": return ("Google Drive", ["com.google.drivefs"])
    case "Box": return ("Box", ["com.box.desktop"])
    default: return nil
    }
}

if action == "sync" {
    // A File Provider store only syncs while its host app is running; with it
    // quit, nothing new comes down and edits made elsewhere never land. There's
    // no public API for a third-party app to force a provider to re-sync, so
    // the real nudge is making sure the app is up.
    var launched: String?
    if !isICloud, let (name, ids) = providerApp(for: url) {
        let running = ids.contains { !NSRunningApplication.runningApplications(withBundleIdentifier: $0).isEmpty }
        if !running {
            guard let appURL = ids.lazy.compactMap({ NSWorkspace.shared.urlForApplication(withBundleIdentifier: $0) }).first else {
                fail("\(name) isn't installed, so this folder can't sync")
            }
            let open = Process()
            open.executableURL = URL(fileURLWithPath: "/usr/bin/open")
            open.arguments = ["-g", appURL.path] // background: don't steal focus
            do { try open.run(); open.waitUntilExit() } catch { fail("couldn't start \(name): \(error.localizedDescription)") }
            if open.terminationStatus != 0 { fail("couldn't start \(name)") }
            launched = name
        }
    }
    // A coordinated read of the folder asks the File Provider daemon for its
    // current listing (enumerating it if it's still a placeholder), bounded so
    // a wedged provider can't hang the helper.
    let done = DispatchSemaphore(value: 0)
    DispatchQueue.global().async {
        var cerr: NSError?
        NSFileCoordinator().coordinate(readingItemAt: url, options: [], error: &cerr) { u in
            _ = try? fm.contentsOfDirectory(atPath: u.path)
        }
        done.signal()
    }
    _ = done.wait(timeout: .now() + 15)
    print(launched.map { "launched \($0)" } ?? "ok")
    exit(0)
}

if action == "status" {
    // How many of the folder's items are mid-transfer. The ubiquitous-item
    // keys cover iCloud and File Provider stores alike. Hidden files are
    // skipped: providers never sync `.DS_Store` / the `Icon\r` custom-icon
    // file, so those report "uploading" forever.
    let keys: [URLResourceKey] = [.ubiquitousItemIsDownloadingKey, .ubiquitousItemIsUploadingKey]
    let items = (try? fm.contentsOfDirectory(at: url, includingPropertiesForKeys: keys, options: [.skipsHiddenFiles])) ?? []
    var busy = 0
    for u in items where u.lastPathComponent != "Icon\r" {
        guard let v = try? u.resourceValues(forKeys: Set(keys)) else { continue }
        if v.ubiquitousItemIsDownloading == true || v.ubiquitousItemIsUploading == true {
            busy += 1
        }
    }
    print("busy \(busy)")
    exit(0)
}

let sem = DispatchSemaphore(value: 0)
var failure: String?

switch (action, isICloud) {
case ("download", true):
    do { try fm.startDownloadingUbiquitousItem(at: url) } // async in the iCloud daemon
    catch { failure = "download: \(error.localizedDescription)" }
    sem.signal()

case ("evict", true):
    do { try fm.evictUbiquitousItem(at: url) }
    catch { failure = "evict: \(error.localizedDescription)" }
    sem.signal()

case ("download", false):
    // File Provider: a coordinated read faults the file in via its extension.
    let coord = NSFileCoordinator()
    var cerr: NSError?
    coord.coordinate(readingItemAt: url, options: [], error: &cerr) { u in
        _ = try? Data(contentsOf: u, options: .mappedIfSafe)
    }
    if let e = cerr { failure = "download: \(e.localizedDescription)" }
    sem.signal()

case ("evict", false):
    // Third-party File Provider stores don't expose a reliable public evict;
    // that's driven from the provider's own app. Report it so Shuffle can tell
    // the user (it only offers "Free Up Space" for iCloud).
    fail("evict: freeing space is only supported for iCloud Drive here; use the provider's app for this store")

default:
    fail("unreachable")
}

// Bound the wait so a wedged provider can't hang Shuffle's helper forever.
if sem.wait(timeout: .now() + 30) == .timedOut {
    fail("timed out talking to the cloud provider")
}
if let failure = failure { fail(failure) }
print("ok")
