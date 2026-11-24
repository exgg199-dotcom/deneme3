"""Run the production native launch gate and resource-only ZIP import on scratch files."""
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest
from test_mobile_game_files_keychain import swift_block

ROOT = Path(__file__).resolve().parents[1]


class GameFilesRecoveryTests(unittest.TestCase):
    def test_missing_files_gate_and_resource_only_import(self):
        if not shutil.which('swiftc'):
            self.skipTest('swiftc required')
        link = (ROOT / 'packaging/ios/Launcher/GameLink.swift').read_text()
        files = (ROOT / 'packaging/ios/Launcher/GameFiles.swift').read_text()
        gate = swift_block(link, link.index('    private func beginLaunch('))
        needs_files = swift_block(link, link.index('    var needsGameFiles:'))
        staging = swift_block(files, files.index('struct ArchiveStaging')).replace(
            'FileManager.default.temporaryDirectory', 'URL(fileURLWithPath: CommandLine.arguments[1])')
        stage = swift_block(files, files.index('    nonisolated static func stageArchive('))
        import_message = swift_block(files, files.index('    nonisolated static func fileImportMessage('))
        importer = swift_block(files, files.index('    func importPreparedFile('))
        finished = swift_block(files, files.index('    private func finished('))
        failure = swift_block(files, files.index('    var automaticFailure:'))
        complete = swift_block(files, files.index('    private func completeImport('))
        message = swift_block(files, files.index('    private static func importMessage('))
        source = r'''
import CoreFoundation
import Foundation
enum Phase: Equatable {case waiting,title,world,loading,needsFiles(String?)}
enum GameFolder {static var hasGameFiles=false}
final class LaunchAttempt {}
final class Gate {
 var phase=Phase.waiting;var requestingGameFiles=false;var cleaningUpCache=false
 var hasGameFiles:Bool{GameFolder.hasGameFiles}
 var launching:String?;var currentLaunch:LaunchAttempt?;var phaseEpoch=UUID()
 NEEDS_FILES
 GATE
 func launch()->Bool {beginLaunch("Play") != nil}
}
extension Dictionary where Key == String, Value == Any {var error:String? {self["error"] as? String}}
final class URLSession {func finishTasksAndInvalidate(){}}
enum Haptics {static func failed(){};static func done(){}}
struct DownloadError: LocalizedError {let errorDescription:String?;init(_ message:String){errorDescription=message}}
enum Diagnostics {static func redact(_ text:String)->String{text}}
STAGING
@MainActor final class Importer {
 var startedAt = 0.0;var diagnosticEvents:[String]=[]
 func trace(_ text:String){diagnosticEvents.append(text)}
 func traceStorage(){}
 static func errorFacts(_ error:Error)->String{String((error as NSError).code)}
 nonisolated static let maximumArchiveBytes:Int64=1024
 nonisolated static let archiveLimitMessage="The archive exceeds the limit."
 var retryRequest:URLRequest?
 func startTransfer(_ request:URLRequest){preconditionFailure("Unexpected retry: \(request)")}
 FILE_IMPORT_MESSAGE
 enum State:Equatable {case idle,preparingArchive,cancellingArchive,installing,done(String),failed(String)}
 var session:URLSession?;var stagingCopy:Task<Void,Error>?;var rejectedMarker:String?
 var receivedBytes=0,expectedBytes=0
 var resourcesOnly=false
 func endBackground(){}
 func keepInstalledGameFiles()->Bool{GameFolder.hasGameFiles}
 var busy=false;var automaticDownload=false;var attempt=UUID();var state=State.idle;var calls=[String]();var reject=false
 var successReply: [String:Any] = ["ok":true,"kind":"resources"]
 var stagedFiles:[String]{calls.map{String($0.drop(while:{!$0.isWhitespace}).dropFirst())}}
 STAGE_ARCHIVE
 func importArchive(_ line:String) async->[String:Any] {
  precondition(state == .installing);precondition(FileManager.default.fileExists(atPath:String(line.drop(while: { !$0.isWhitespace }).dropFirst())))
  calls.append(line);return reject ? ["error":"Not prepared resources"] : successReply
 }
 COMPLETE
 MESSAGE
 FAILURE
 IMPORTER
 FINISHED
 func completeDownload(_ file:URL) async {
  let source=URLSession()
  session=source
  await finished(.success(file),from:source)
 }
 func failDownload() async {
  let source=URLSession()
  session=source
  await finished(.failure(DownloadError("Connection refused")),from:source)
 }
}
@main struct Probe {
 static func main() async throws {
  for phase in [Phase.waiting,.needsFiles(nil),.needsFiles("Missing manifest")] {
   let g=Gate();g.phase=phase;GameFolder.hasGameFiles=false
   precondition(!g.launch());precondition(g.requestingGameFiles);precondition(g.launching == nil)
  }
  let rejected=Gate();rejected.phase = .needsFiles("Rejected");GameFolder.hasGameFiles=true
  precondition(!rejected.launch());precondition(rejected.requestingGameFiles)
  for phase in [Phase.waiting,.title,.world,.loading] {
   let g=Gate();g.phase=phase;GameFolder.hasGameFiles=true
   precondition(g.launch());precondition(!g.requestingGameFiles)
  }
  let active=Gate();active.launching="Starting";precondition(!active.launch());precondition(!active.requestingGameFiles)
  let url=URL(fileURLWithPath:CommandLine.arguments[1]).appendingPathComponent("source.zip")
  try Data("fixture".utf8).write(to:url)
  let i=Importer();await i.importPreparedFile(url)
  guard case .done = i.state else {preconditionFailure("Expected installed resources")};precondition(i.calls.count == 1)
  precondition(i.calls[0].hasPrefix("import-game-files "));precondition(i.stagedFiles.allSatisfy{!FileManager.default.fileExists(atPath:$0)})
  precondition(FileManager.default.fileExists(atPath:url.path))
  i.automaticDownload=true;i.state = .failed("Connection refused")
  precondition(i.automaticFailure == "Connection refused")
  i.state = .idle;precondition(i.automaticFailure == nil)
  i.state = .installing;precondition(i.automaticFailure == nil)
  i.state = .failed("Custom-link failure");i.automaticDownload=false
  precondition(i.automaticFailure == nil)
  i.automaticDownload=true
  i.reject=true;await i.importPreparedFile(url);precondition(i.state == .failed("Not prepared resources"));precondition(i.automaticFailure == nil)
  precondition(i.stagedFiles.allSatisfy{!FileManager.default.fileExists(atPath:$0)})
  // The file limit is enforced before copying/importing a ZIP.
  try Data(repeating: 1, count: 1025).write(to:url)
  await i.importPreparedFile(url)
  if case .failed = i.state {} else {preconditionFailure("Oversized files must fail")}
  precondition(i.calls.count == 2)
  precondition(i.stagedFiles.allSatisfy{!FileManager.default.fileExists(atPath:$0)})
  i.busy=true;await i.importPreparedFile(url);precondition(i.calls.count == 2)
  i.busy=false;await i.importPreparedFile(url.appendingPathComponent("missing"))
  if case .failed = i.state {} else {preconditionFailure("File errors must be visible")}

  let downloaded=URL(fileURLWithPath:CommandLine.arguments[1]).appendingPathComponent("download.zip")

  try Data("fixture".utf8).write(to:url)
  for (reply, ready) in [
    ([:], true),
    (["ok":false,"kind":"resources"], true),
    (["ok":1,"kind":"resources"], true),
    (["ok":true,"kind":"unknown"], true),
    (["ok":true,"kind":"world","name":"Unexpected"], true),
    (["ok":true,"kind":"resources"], false),
  ] as [([String:Any],Bool)] {
   GameFolder.hasGameFiles=ready
   let local=Importer()
   local.successReply=reply
   await local.importPreparedFile(url)
   guard case .failed = local.state else {preconditionFailure("Local import reported success for \(reply)")}
   let remote=Importer()
   remote.resourcesOnly=true
   remote.successReply=reply
   try Data("download".utf8).write(to:downloaded)
   await remote.completeDownload(downloaded)
   guard case .failed = remote.state else {preconditionFailure("Downloaded import reported success for \(reply)")}
  }
  for (kind, confirmation) in [("world","World Meadow added."),("resource_pack","Resource pack Meadow added.")] {
   GameFolder.hasGameFiles=false
   let remote=Importer()
   remote.successReply=["ok":true,"kind":kind,"name":"Meadow"]
   try Data("download".utf8).write(to:downloaded)
   await remote.completeDownload(downloaded)
   precondition(remote.state == .done(confirmation))
  }
  GameFolder.hasGameFiles=true
  for (automatic, onlyGameFiles, command) in [
    (false,true,"import-game-files "), (false,false,"import "), (true,true,"")
  ] {
   try Data("download".utf8).write(to:downloaded)
   let transfer=Importer()
   transfer.automaticDownload=automatic
   transfer.resourcesOnly=onlyGameFiles
   await transfer.completeDownload(downloaded)
   if command.isEmpty {
    precondition(transfer.calls.isEmpty,"automatic setup preserves an existing installation")
   } else {
    precondition(transfer.calls.count == 1 && transfer.calls[0].hasPrefix(command))
   }
   precondition(!FileManager.default.fileExists(atPath:downloaded.path))
  }
  for (automatic, ready) in [(true,true),(true,false),(false,true)] {
   GameFolder.hasGameFiles=ready
   let transfer=Importer()
   transfer.automaticDownload=automatic
   await transfer.failDownload()
   if automatic && ready {
    precondition(transfer.state == .done("Game files are already installed."))
    precondition(transfer.automaticFailure == nil)
   } else {
    precondition(transfer.state == .failed("Connection refused"))
   }
   precondition(transfer.calls.isEmpty)
   precondition(transfer.diagnosticEvents.contains { $0.hasPrefix("transfer_failed") })
  }
 }
}
'''
        source = source.replace('FILE_IMPORT_MESSAGE', import_message).replace('STAGING', staging).replace('STAGE_ARCHIVE', stage).replace('NEEDS_FILES', needs_files).replace('GATE', gate).replace('IMPORTER', importer).replace('FINISHED', finished).replace('FAILURE', failure).replace('COMPLETE', complete).replace('MESSAGE', message)
        with tempfile.TemporaryDirectory(prefix='horizon-recovery-') as directory:
            probe = Path(directory) / 'probe.swift'
            binary = Path(directory) / 'probe'
            probe.write_text(source)
            build = subprocess.run(['swiftc', '-parse-as-library', str(probe), '-o', str(binary)], capture_output=True, text=True)
            self.assertEqual(build.returncode, 0, build.stdout + build.stderr)
            run = subprocess.run([str(binary), directory], capture_output=True, text=True)
            self.assertEqual(run.returncode, 0, run.stdout + run.stderr)
