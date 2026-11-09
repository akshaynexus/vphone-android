// vphone-android: the desktop installer path (sshfs, sudo, kernel modules, rcodesign) is not built.
// On Android `cfw install` is android/create/installer's vphone-create-steps, which drives the SSH
// ramdisk itself and calls this executable's individual `cfw` patch verbs.
import ArgumentParser
import FirmwarePatcher
import Foundation
import VPhoneCoreKit
import VPhonePatchKit

// Referenced by desktop-only branches of VPhoneCustomFirmwareInstaller.swift, which `run()` never reaches on Android.
/// The vphone-linux checkout: $VPHONE_LINUX_ROOT, or the first directory above
/// this binary (it lives in linux-create/.build/...) that holds run-vphone.sh.
func vphoneLinuxRoot() -> String {
    if let root = ProcessInfo.processInfo.environment["VPHONE_LINUX_ROOT"], !root.isEmpty {
        return root
    }
    var dir = URL(fileURLWithPath: CommandLine.arguments[0]).resolvingSymlinksInPath()
        .deletingLastPathComponent()
    while dir.path != "/" {
        if FileManager.default.fileExists(atPath: dir.appendingPathComponent("run-vphone.sh").path) {
            return dir.path
        }
        dir = dir.deletingLastPathComponent()
    }
    return FileManager.default.currentDirectoryPath
}

/// rcodesign: $VPHONE_RCODESIGN, then PATH, then ~/.cargo/bin of the user who ran
/// sudo (cargo installs it there), then root's.
func rcodesignPath() -> String {
    let env = ProcessInfo.processInfo.environment
    var candidates = [env["VPHONE_RCODESIGN"]].compactMap { $0 }
    for dir in (env["PATH"] ?? "").split(separator: ":") {
        candidates.append("\(dir)/rcodesign")
    }
    if let user = env["SUDO_USER"], let pw = getpwnam(user), let home = pw.pointee.pw_dir {
        candidates.append(String(cString: home) + "/.cargo/bin/rcodesign")
    }
    if let home = env["HOME"] { candidates.append(home + "/.cargo/bin/rcodesign") }
    return candidates.first { FileManager.default.isExecutableFile(atPath: $0) } ?? "rcodesign"
}

extension VPhoneCustomFirmwareInstaller {
    func runLinuxGuest(
        bundleDirectory: VPhoneConfinedDirectory,
        bundlePath: String,
        restore: VPhoneConfinedDirectory?,
        plan: VPhoneVirtualMachinePatchPlan?,
        receipt: VPhoneVirtualMachinePatchReceipt?,
        receiptGuest: Set<String>?,
        receiptKernel: Set<String>?,
        priorFallback: Set<String>,
        invokingUser: VPhoneInvokingUser?,
        callerUID: uid_t?,
    ) throws {
        throw ValidationError("`cfw install` is run by the Android app's installer, not by vphone-cli.")
    }
}
