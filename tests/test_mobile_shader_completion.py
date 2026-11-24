"""Completed shader jobs quarantine failed packs, without disabling pending/geometry jobs.

Run the production completion handler and quarantine method with a renderer
completion queue and settings persistence recorder; no shader assets or GPU.
"""
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest
from test_mobile_command_keyboard import method

ROOT = Path(__file__).resolve().parents[1]


def program():
    source = (ROOT / 'crates/horizon-client/src/game/interface/shader_packs.rs').read_text()
    helpers = '\n'.join(method(source, name) for name in ('clear_shader_report', 'shader_failure_notes', 'complete_parser'))
    game = '\n'.join(method(source, name) for name in ('poll_shader_workers', 'shader_pack_failed'))
    matches = '\n'.join(method(source, name) for name in ('matches_report', 'retry_for_target'))
    return STUBS + '\n' + helpers + '\nimpl ClassicPack {\n' + matches + '\n}\nimpl Game {\n' + game + '\n}\n' + CASES


STUBS = r'''
#![allow(dead_code)]
extern crate self as log;
#[macro_export]macro_rules! warn{($($t:tt)*)=>{{}}}
#[macro_export]macro_rules! error{($($t:tt)*)=>{{}}}
use std::{cell::{Cell,RefCell},path::{Path,PathBuf},sync::{mpsc,Arc,Mutex,Weak}};
type Report=(i32,Vec<String>,Vec<String>);
struct ShaderPackReport{
 dimension:i32, programs:Vec<String>, notes:Vec<String>, failed_target_size:Option<[u32;2]>,
}
impl From<Report> for ShaderPackReport {
 fn from((dimension,programs,notes):Report)->Self{
  Self{dimension,programs,notes,failed_target_size:None}
 }
}
macro_rules! json {
 ({"pack": $pack:expr, "dimension": $dimension:expr, "notes": $notes:expr}) => {
    ($pack.to_owned(), $dimension, $notes)
 };
}
thread_local!{static REMEMBERED:RefCell<Vec<(PathBuf,bool)>>=const{RefCell::new(Vec::new())};}
fn fingerprint(_:&Path)->Option<u64>{Some(42)}
fn remember(path:&Path,stamp:Option<u64>,api:bool){assert_eq!(stamp,Some(42));REMEMBERED.with_borrow_mut(|r|r.push((path.to_path_buf(),api)));}

thread_local!{
 static OPTIONS_REVISION:Cell<u64>=const{Cell::new(0)};
 static PENDING_LOAD:RefCell<Option<PendingLoad>>=const{RefCell::new(None)};
 static LAST_DISABLED:RefCell<Option<(String,i32,Vec<String>)>>=const{RefCell::new(None)};
 static REPORT:RefCell<Option<(String,Report)>>=const{RefCell::new(None)};
}
#[derive(PartialEq)]struct LoadKey{path:PathBuf,stamp:Option<u64>,resources:PathBuf,options_revision:u64,dimension:i32,dimension_name:String,context:(u64,u64)}
type ParsedPack=();
struct ParserCompletion{report_name:String,key:LoadKey,result:Result<ParsedPack,String>}
#[derive(Default)]struct ParserInbox(Arc<Mutex<Option<ParserCompletion>>>);
impl ParserInbox{fn lock(&self)->std::sync::MutexGuard<'_,Option<ParserCompletion>>{self.0.lock().unwrap()}}
struct PendingLoad{inbox:Weak<Mutex<Option<ParserCompletion>>>,report_name:String,key:LoadKey,worker:Option<std::thread::JoinHandle<()>>,result:mpsc::Receiver<Result<ParsedPack,String>>}
#[derive(Clone,Copy)]enum Dim{Overworld,Nether,End,Extra(u32)}
impl Dim{fn id(self)->&'static str{match self{Self::Overworld=>"minecraft:overworld",Self::Extra(_)=>"server:alternate",Self::Nether=>"minecraft:the_nether",Self::End=>"minecraft:the_end"}}}
struct Renderer{dimension:Option<i32>,report:Option<ShaderPackReport>,dropped:bool,disabled:bool}
impl Renderer{
 fn shader_pack_dimension(&self)->Option<i32>{self.dimension}
 fn drop_shader_packs(&mut self){self.report=None;self.dropped=true;}
 fn take_shader_report(&mut self)->Option<ShaderPackReport>{self.report.take()}
 fn shader_context_key(&self)->(u64,u64){(0,0)}
 fn set_shader_pack(&mut self,dimension:i32,_:Option<()>,_:&[i32]){self.disabled=true;self.dimension=Some(dimension);self.report=None;}
}
struct Horizon{shader_pack:String,saves:usize}
impl Horizon{
 fn select_shader_pack(&mut self,_:&Path,pack:String)->Result<(),std::io::Error>{
  self.shader_pack=pack;
  self.saves+=1;
  Ok(())
 }
}
struct Settings{horizon:Horizon}
fn picked_pack(s:&Settings)->&str{&s.horizon.shader_pack}
fn dev_pack()->Option<String>{None}
struct ClassicPack{path:PathBuf,stamp:Option<u64>,dimension_name:&'static str,pending_ids:Option<()>,refused:bool,failed_target_size:Option<[u32;2]>}
struct NativeShaders{picked:String,classic:Option<ClassicPack>}
impl NativeShaders{fn resolve_path(&mut self,dir:&std::path::Path,picked:&str,_:bool)->PathBuf{if picked=="horizon:example"{dir.join("example.zip")}else{dir.join(picked)}}}
struct Options{resources:PathBuf}
struct Text;impl Text{fn colored(s:String,_:u32)->String{s}}
struct Game{shader_parser:ParserInbox,dim:Dim,renderer:Option<Renderer>,pack_ids:Option<()>,native_shaders:NativeShaders,settings:Settings,options:Options,messages:Vec<String>}
impl Game{
 fn shader_packs_dir()->PathBuf{PathBuf::from("/fixture/shaderpacks")}
 fn game_dir()->PathBuf{PathBuf::from("scratch")}
 fn system_message(&mut self,text:String){self.messages.push(text);}
 fn new(report:Option<Report>)->Self{
  let path=PathBuf::from("/fixture/shaderpacks/example.zip");
  let classic=Some(ClassicPack{stamp:fingerprint(&path),path:path.clone(),dimension_name:"minecraft:overworld",pending_ids:Some(()),refused:false,failed_target_size:None});
  Self{shader_parser:ParserInbox::default(),dim:Dim::Overworld,renderer:Some(Renderer{dimension:Some(0),report:report.map(Into::into),dropped:false,disabled:false}),pack_ids:Some(()),native_shaders:NativeShaders{picked:path.to_string_lossy().into_owned(),classic},settings:Settings{horizon:Horizon{shader_pack:"example.zip".into(),saves:0}},options:Options{resources:PathBuf::from("scratch/resources")},messages:vec![]}}
}
// This fixture records finished reports only; there is no pending notice to clear.
fn clear_pending_shader_report(_: &str, _: i32){}
fn record_report(name:&str,dimension:i32,programs:&[String],notes:&[String]){REPORT.with_borrow_mut(|r|*r=Some((name.into(),(dimension,programs.into(),notes.into()))));}
'''

CASES = r'''
#[test]fn resolved_pack_identity_keeps_a_matching_parser_result(){
 let mut g=Game::new(None);g.settings.horizon.shader_pack="horizon:example".into();
 let(tx,rx)=mpsc::channel();tx.send(Ok(())).unwrap();
 PENDING_LOAD.with_borrow_mut(|pending|*pending=Some(PendingLoad{inbox:Arc::downgrade(&g.shader_parser.0),report_name:"example.zip".into(),key:LoadKey{stamp:Some(42),path:PathBuf::from("/fixture/shaderpacks/example.zip"),resources:g.options.resources.clone(),options_revision:OPTIONS_REVISION.with(Cell::get),dimension:0,dimension_name:"minecraft:overworld".into(),context:(0,0)},worker:None,result:rx}));
 let mut other=Game::new(None);
 other.settings.horizon.shader_pack="replacement.zip".into();
 other.poll_shader_workers();
 assert!(other.shader_parser.lock().is_none());
 PENDING_LOAD.with_borrow(|pending|assert!(pending.is_none(),"the parser slot is free for another renderer"));
 g.poll_shader_workers();
 assert!(g.shader_parser.lock().take().unwrap().result.is_ok());
 assert!(!g.renderer.as_ref().unwrap().dropped);
}

#[test]fn failure_from_previous_selection_does_not_disable_the_new_pack(){
 for selection in ["replacement.zip", ""] {
 let mut g=Game::new(Some((0,vec![],vec!["old pack failed".into()])));
 g.settings.horizon.shader_pack=selection.into();
 g.poll_shader_workers();
 assert_eq!(g.settings.horizon.shader_pack,selection);
 assert_eq!(g.settings.horizon.saves,0);
 assert!(g.messages.is_empty());
 assert!(g.renderer.as_ref().unwrap().dropped);
 }
}

#[test]fn failed_completed_gpu_job_is_disabled_saved_and_reported(){
 let notes=vec!["Device attachment limit exceeded".to_owned(),"Using normal rendering".to_owned()];
 let mut g=Game::new(Some((0,vec![],notes.clone())));g.poll_shader_workers();
 assert!(g.pack_ids.is_none());assert!(g.settings.horizon.shader_pack.is_empty());assert_eq!(g.settings.horizon.saves,1);
 assert!(g.renderer.as_ref().unwrap().disabled);assert_eq!(g.messages.len(),1);
 REMEMBERED.with_borrow(|r|assert!(r.iter().all(|(p,api)|p==&PathBuf::from("/fixture/shaderpacks/example.zip")&&!*api)));
 REPORT.with_borrow(|r|assert_eq!(r.as_ref().unwrap(),&("example.zip".into(),(0,vec![],notes.clone()))));
 LAST_DISABLED.with_borrow(|r|assert_eq!(*r,Some(("example.zip".into(),0,notes))));
 g.poll_shader_workers();assert_eq!(g.settings.horizon.saves,1);assert_eq!(g.messages.len(),1);
}
#[test]fn replaced_file_does_not_quarantine_current_pack(){
 let mut g=Game::new(Some((0,vec![],vec!["stale failure".into()])));
 g.native_shaders.classic.as_mut().unwrap().stamp=Some(41);
 g.poll_shader_workers();
 assert!(g.renderer.as_ref().unwrap().dropped);
 assert_eq!(g.settings.horizon.shader_pack,"example.zip");
 assert_eq!(g.settings.horizon.saves,0);
 assert!(g.messages.is_empty());
}
#[test]fn pending_gpu_job_keeps_selected_pack(){let mut g=Game::new(None);g.poll_shader_workers();assert_eq!(g.settings.horizon.shader_pack,"example.zip");assert_eq!(g.settings.horizon.saves,0);assert!(g.messages.is_empty());}
#[test]fn geometry_only_completed_pack_is_not_quarantined(){let mut g=Game::new(Some((0,vec!["gbuffers_terrain".into()],vec![])));g.poll_shader_workers();assert!(g.pack_ids.is_some());assert_eq!(g.settings.horizon.shader_pack,"example.zip");assert_eq!(g.settings.horizon.saves,0);assert!(!g.renderer.as_ref().unwrap().disabled);REMEMBERED.with_borrow(|r|assert_eq!(r.last().unwrap().1,true));}
#[test]fn a_resize_failure_keeps_the_selection_and_retries_at_a_new_target_size(){
 let mut g=Game::new(Some((0,vec!["gbuffers_terrain".into()],vec![])));
 g.poll_shader_workers();
 assert!(g.pack_ids.is_some());
 assert!(g.native_shaders.classic.as_ref().unwrap().pending_ids.is_none());
 g.renderer.as_mut().unwrap().report=Some(ShaderPackReport{
  dimension:0,programs:vec![],notes:vec!["resize failure".into()],failed_target_size:Some([2000,1200]),
 });
 g.poll_shader_workers();
 assert_eq!(g.settings.horizon.shader_pack,"example.zip");
 assert_eq!(g.settings.horizon.saves,0);
 assert!(g.pack_ids.is_none());
 let fallback=g.native_shaders.classic.as_ref().unwrap();
 assert!(fallback.refused);
 assert!(!fallback.retry_for_target([2000,1200]));
 assert!(fallback.retry_for_target([1200,2000]));
 assert!(g.messages.is_empty());
 REPORT.with_borrow(|r|assert_eq!(r.as_ref().unwrap().1.2,["resize failure"]));
 LAST_DISABLED.with_borrow(|r|assert!(r.is_none()));
}
#[test]fn changing_named_dimensions_releases_a_refusal_in_the_same_legacy_slot(){
 let mut g=Game::new(None);
 let refused=g.native_shaders.classic.as_mut().unwrap();
 refused.refused=true;
 refused.pending_ids=None;
 g.dim=Dim::Extra(1);
 g.poll_shader_workers();
 assert!(g.renderer.as_ref().unwrap().dropped);
 assert!(g.native_shaders.classic.is_none());
 assert_eq!(g.settings.horizon.shader_pack,"example.zip");
 assert_eq!(g.settings.horizon.saves,0);
}
#[test]fn obsolete_dimension_failure_cannot_disable_current_pack(){let mut g=Game::new(Some((-1,vec![],vec!["old failure".into()])));g.renderer.as_mut().unwrap().dimension=Some(-1);g.poll_shader_workers();assert!(g.renderer.as_ref().unwrap().dropped);assert_eq!(g.settings.horizon.shader_pack,"example.zip");assert_eq!(g.settings.horizon.saves,0);assert!(g.messages.is_empty());}
'''


class MobileShaderCompletionTests(unittest.TestCase):
    def test_production_gpu_completion_and_quarantine(self):
        compiler = shutil.which('rustc')
        if compiler is None:
            self.skipTest('rustc is required')
        with tempfile.TemporaryDirectory(prefix='horizon-shader-completion-') as folder:
            source = Path(folder) / 'probe.rs'
            binary = Path(folder) / 'probe'
            source.write_text(program())
            build = subprocess.run([compiler, '--edition=2024', '--test', str(source), '-o', str(binary)], capture_output=True, text=True, timeout=60)
            self.assertEqual(build.returncode, 0, build.stdout + build.stderr)
            run = subprocess.run([str(binary)], capture_output=True, text=True, timeout=10)
            self.assertEqual(run.returncode, 0, run.stdout + run.stderr)
            self.assertIn(f'{CASES.count("#[test]")} passed', run.stdout)
