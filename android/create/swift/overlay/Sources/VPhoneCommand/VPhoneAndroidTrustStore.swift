// vphone-android: TLS roots for the statically linked libcurl (TSS requests in libtatsu).
//
// Android keeps its roots as one PEM per file, named by OpenSSL's old subject hash, which OpenSSL
// 3's CApath lookup does not use. So the system store is concatenated once into a bundle in the
// app's temporary directory and handed to OpenSSL through SSL_CERT_FILE; libcurl was configured
// with --with-ca-fallback to reach it. FoundationNetworking finds the store by itself.
import Foundation

func vphoneAndroidPrepareEnvironment() {
    // Foundation takes the home directory from getpwuid, "/" for an app uid, before $HOME.
    if let home = ProcessInfo.processInfo.environment["HOME"], !home.isEmpty { setenv("CFFIXED_USER_HOME", home, 0) }
    guard ProcessInfo.processInfo.environment["SSL_CERT_FILE"] == nil else { return }
    let fm = FileManager.default
    let bundle = URL(fileURLWithPath: NSTemporaryDirectory()).appendingPathComponent("vphone-system-roots.pem")
    if !fm.fileExists(atPath: bundle.path) {
        var pem = Data()
        // The conscrypt APEX holds the updatable store; /system is the fallback on older images.
        for directory in ["/apex/com.android.conscrypt/cacerts", "/system/etc/security/cacerts"] {
            guard let names = try? fm.contentsOfDirectory(atPath: directory), !names.isEmpty else { continue }
            for name in names.sorted() where name.hasSuffix(".0") {
                guard let certificate = fm.contents(atPath: directory + "/" + name) else { continue }
                pem.append(certificate)
                pem.append(0x0A)
            }
            break
        }
        guard !pem.isEmpty else { return }
        try? fm.createDirectory(at: bundle.deletingLastPathComponent(), withIntermediateDirectories: true)
        try? pem.write(to: bundle, options: .atomic)
    }
    setenv("SSL_CERT_FILE", bundle.path, 1)
}
