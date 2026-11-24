"""Production thumbnail coalescing and retry with deterministic fake image IO.

Only UIKit's file/image decoder and Published are doubled; the actual cache and
async completion code run unchanged, without reading player screenshots.
"""
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest
from test_mobile_game_files_keychain import swift_block

ROOT = Path(__file__).resolve().parents[1]


def program(source=None):
    source = source or (ROOT / "packaging/ios/Launcher/Gallery.swift").read_text()
    start = source.index("@MainActor\nfinal class Thumbnails")
    cache = swift_block(source, start)
    decoder = swift_block(cache, cache.index("    nonisolated private static func decode("))
    cache = cache.replace(decoder, '''
    nonisolated private static func decode(_ shot: Shot, longestSide: Int) -> UIImage? {
        ImageFixture.open() ? UIImage() : nil
    }
    ''')
    return PREAMBLE + cache + '''
extension Thumbnails {
    func pending(_ shot: Shot) -> Bool { asked.contains(shot) }
}
''' + CASES


PREAMBLE = r'''
import Foundation
protocol ObservableObject {}
@propertyWrapper struct Published<T>{var wrappedValue:T}
struct Shot:Hashable{let url:URL}
enum ImageFixture {
 static let condition=NSCondition()
 static var available=false
 static var reads=0
 static var blocked=false
 static func set(_ value:Bool){condition.lock();defer{condition.unlock()};available=value;reads=0}
 static func open()->Bool{condition.lock();defer{condition.unlock()};reads+=1;while blocked{condition.wait()};return available}
 static func total()->Int{condition.lock();defer{condition.unlock()};return reads}
 static func block(){condition.lock();blocked=true;condition.unlock()}
 static func release(){condition.lock();blocked=false;condition.broadcast();condition.unlock()}
}
final class UIImage:@unchecked Sendable {}
'''
CASES = r'''
@main struct Probe {
 @MainActor static var failures=0
 @MainActor static func expect(_ value:Bool,_ message:String){if !value{failures+=1;print("FAIL: \(message)")}}
 @MainActor static func until(_ predicate:()->Bool) async -> Bool {
  let deadline=Date().addingTimeInterval(3)
  while !predicate(),Date()<deadline{await Task.yield()}
  return predicate()
 }
 @MainActor static func main() async {
  let shot=Shot(url:URL(fileURLWithPath:"/fixture/thumbnail.png"))
  do {
   let model=Thumbnails();model.retain([shot]);ImageFixture.set(false)
   model.load(shot,side:110)
   let completed=await until{!model.pending(shot)}
   expect(completed,"first unavailable image decode completed")
   expect(model.images[shot] == nil,"unavailable image stays blank")
   ImageFixture.set(true);model.load(shot,side:110)
   let loaded=await until{model.images[shot] != nil}
   expect(loaded,"retry must load a now-available image")
   expect(ImageFixture.total()==1,"retry actually reads the now-valid file")
  }
  do {
   let model=Thumbnails();model.retain([shot]);ImageFixture.set(true);ImageFixture.block()
   model.load(shot,side:110);model.load(shot,side:110)
   let started=await until{ImageFixture.total()>0};expect(started,"first image IO starts")
   expect(ImageFixture.total()==1,"simultaneous loads share one in-flight decode")
   expect(model.images[shot] == nil,"blocked decoder has not published")
   ImageFixture.release()
   let loaded=await until{model.images[shot] != nil};expect(loaded,"coalesced decode publishes")
  }
  do {
   let model=Thumbnails();model.retain([shot]);ImageFixture.set(true);model.load(shot,side:110)
   let loaded=await until{model.images[shot] != nil};expect(loaded,"successful initial decode publishes")
   model.load(shot,side:110);model.load(shot,side:110)
   expect(ImageFixture.total()==1,"successful cache avoids further file reads")
  }
  if failures>0{exit(1)}
  print("3 thumbnail retry/cache cases passed")
 }
}
'''


class GalleryThumbnailTests(unittest.TestCase):
    def test_failed_thumbnail_can_retry_without_duplicating_successful_work(self):
        compiler = shutil.which("swiftc")
        if not compiler:
            self.skipTest("swiftc is required for production thumbnail checks")
        with tempfile.TemporaryDirectory(prefix="horizon-gallery-thumbnail-") as folder:
            source = Path(folder) / "probe.swift"
            binary = Path(folder) / "probe"
            source.write_text(program())
            built = subprocess.run([compiler, "-parse-as-library", str(source), "-o", str(binary)],
                                   capture_output=True, text=True, timeout=60)
            self.assertEqual(built.returncode, 0, built.stdout + built.stderr)
            run = subprocess.run([str(binary)], capture_output=True, text=True, timeout=15)
            self.assertEqual(run.returncode, 0, run.stdout + run.stderr)
            self.assertIn("3 thumbnail retry/cache cases passed", run.stdout)
