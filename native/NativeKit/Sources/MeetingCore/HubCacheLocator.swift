import Foundation

/// Finds models already downloaded by `huggingface_hub` (Python tools, mlx-audio) so the engine
/// can reuse them instead of downloading a second copy into speech-swift's own cache.
public enum HubCacheLocator {
    /// Candidate hub cache roots, most specific first, following `huggingface_hub`'s variables.
    /// - Parameters:
    ///   - environment: Process environment.
    ///   - home: The user's home directory.
    /// - Returns: Absolute paths, without duplicates.
    public static func cacheRoots(environment: [String: String], home: String) -> [String] {
        var roots: [String] = []
        if let hub = environment["HF_HUB_CACHE"], !hub.isEmpty { roots.append(hub) }
        if let hfHome = environment["HF_HOME"], !hfHome.isEmpty { roots.append(hfHome + "/hub") }
        roots.append(home + "/.cache/huggingface/hub")
        var seen = Set<String>()
        return roots.filter { seen.insert($0).inserted }
    }

    /// Locates the snapshot directory of a repo that has its weights on disk.
    /// - Parameters:
    ///   - repo: Hub repo id, e.g. `mlx-community/Qwen3-ASR-1.7B-8bit`.
    ///   - environment: Process environment.
    ///   - home: Home directory.
    ///   - readFile: Reads a small text file (the `refs/main` revision pointer).
    ///   - fileExists: Tests a path.
    /// - Returns: The snapshot directory containing `model.safetensors`, or `nil`.
    public static func snapshot(repo: String, environment: [String: String], home: String,
                                readFile: (String) -> String?, fileExists: (String) -> Bool) -> URL? {
        let folder = "models--" + repo.replacingOccurrences(of: "/", with: "--")
        for root in cacheRoots(environment: environment, home: home) {
            let repoDir = "\(root)/\(folder)"
            guard let revision = readFile("\(repoDir)/refs/main")?.trimmingCharacters(in: .whitespacesAndNewlines),
                  !revision.isEmpty
            else { continue }
            let dir = "\(repoDir)/snapshots/\(revision)"
            if fileExists("\(dir)/model.safetensors") || fileExists("\(dir)/model.safetensors.index.json") {
                return URL(filePath: dir, directoryHint: .isDirectory)
            }
        }
        return nil
    }

    /// ``snapshot(repo:environment:home:readFile:fileExists:)`` against the real filesystem.
    /// - Parameter repo: Hub repo id.
    /// - Returns: The snapshot directory, or `nil`.
    public static func snapshot(repo: String) -> URL? {
        snapshot(repo: repo, environment: ProcessInfo.processInfo.environment,
                 home: FileManager.default.homeDirectoryForCurrentUser.path,
                 readFile: { try? String(contentsOfFile: $0, encoding: .utf8) },
                 fileExists: { FileManager.default.fileExists(atPath: $0) })
    }
}
