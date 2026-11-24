"""Held physical touch input must not trigger the game's AFK FPS cap."""
from pathlib import Path
import subprocess,tempfile,unittest
from test_mobile_command_keyboard import method
import shutil
ROOT=Path(__file__).resolve().parents[1]
class TouchFpsActivityTests(unittest.TestCase):
 @unittest.skipUnless(shutil.which("rustc"), "requires Rust toolchain")
 def test_stationary_hold_is_active_but_release_cancel_and_latches_are_not(self):
  game=(ROOT/'crates/horizon-client/src/game/interface/touch.rs').read_text()
  self.assertIn('self.touch_refresh_activity(playing);',method(game,'touch_frame'))
  window=(ROOT/'crates/horizon-client-input/src/window.rs').read_text()
  policy=window[window.index('/// `InactivityFpsLimit`.'):window.index('/// `isHeavilyThrottled`.')]
  prefix='#[path='+repr(str(ROOT/'crates/horizon-client/src/touch.rs')).replace("'",'"')+'] mod touch;\n'
  actual=method(game,'touch_refresh_activity')
  with tempfile.TemporaryDirectory(prefix='horizon-touch-fps-') as directory:
   p=Path(directory)
   for before in [True,False]:
    refresh='fn touch_refresh_activity(&mut self,_:bool) {}' if before else actual
    (p/'main.rs').write_text(prefix+policy+FIXTURE.replace('REFRESH',refresh))
    build=subprocess.run(['rustc','--edition=2024',str(p/'main.rs'),'-o',str(p/'probe')],capture_output=True,text=True)
    self.assertEqual(build.returncode,0,build.stderr)
    run=subprocess.run([str(p/'probe')],capture_output=True,text=True)
    if before:
     self.assertNotEqual(run.returncode,0,'old event-only activity incorrectly passed')
     self.assertIn('held touch must not be treated as AFK',run.stderr)
    else:self.assertEqual(run.returncode,0,run.stdout+run.stderr)
FIXTURE=r'''
use std::time::{Instant,Duration};
struct Touch {fingers:touch::Touches}
struct Game {touch:Touch,focused:bool,last_input:Instant}
impl Game {REFRESH}
fn fps(g:&Game,start:Instant,option:i32)->i32 {
 framerate_limit(option,FrameContext{inactivity:InactivityFpsLimit::Afk,iconified:false,focused:g.focused,exclusive_fullscreen:false,now_ms:start.elapsed().as_millis() as i64,latest_input_ms:g.last_input.duration_since(start).as_millis() as i64,has_level:true,has_screen:false,has_overlay:false})
}
fn main(){
 for control in [touch::Control::Forward,touch::Control::Joystick] {
  for age in [61,601] {
   let start=Instant::now()-Duration::from_secs(age);
   let mut g=Game{touch:Touch{fingers:Default::default()},focused:true,last_input:start};
   let layout=[(control,[0.,0.,32.,32.])];
   g.touch.fingers.start(1,(16.,4.),0.,&layout);
   g.touch.fingers.frame(age as f64);
   g.touch_refresh_activity(true);
   assert_eq!(fps(&g,start,120),120,"held touch must not be treated as AFK");
   assert_eq!(fps(&g,start,30),30,"explicit user cap remains");
   g.last_input=start;g.focused=false;g.touch_refresh_activity(true);assert_eq!(g.last_input,start);
   g.focused=true;g.touch_refresh_activity(false);assert_eq!(g.last_input,start);
   g.touch.fingers.end(1);g.touch_refresh_activity(true);assert_eq!(g.last_input,start);
   g.touch.fingers.locked=true;g.touch_refresh_activity(true);assert_eq!(g.last_input,start,"latched movement is not a physical contact");
   g.touch.fingers.start(2,(16.,4.),0.,&layout);g.touch.fingers.cancel(2);g.touch_refresh_activity(true);assert_eq!(g.last_input,start);
   assert_eq!(fps(&g,start,120),if age==61{30}else{10},"genuine idle throttling remains");
  }
 }
}
'''
