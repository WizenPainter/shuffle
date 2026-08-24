// cloudctl — download (materialize) or evict (free up space) a cloud file.
//
//   cloudctl download <path>   # pull an online-only file down to disk
//   cloudctl evict    <path>   # remove the local copy, keep it online-only
//
// Handles both iCloud Drive (Foundation ubiquitous APIs) and third-party File
// Provider stores (Dropbox, Google Drive, OneDrive, … via NSFileProviderManager
// / coordinated reads). Shuffle spawns this as a subprocess. Prints "ok" on
// success; a message to stderr and a non-zero exit on failure.
import Foundation
import FileProvider

func fail(_ msg: String) -> Never {
    FileHandle.standardError.write((msg + "\n").data(using: .utf8)!)
    exit(1)
}

let args = CommandLine.arguments
guard args.count >= 3, args[1] == "download" || args[1] == "evict" else {
    fail("usage: cloudctl <download|evict> <path>")
}
let action = args[1]
let url = URL(fileURLWithPath: args[2])
let fm = FileManager.default

// iCloud Drive lives under ~/Library/Mobile Documents; everything else that's a
// placeholder is a third-party File Provider store.
let isICloud = url.path.contains("/Library/Mobile Documents/")

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
