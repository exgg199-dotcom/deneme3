"""Client native references preserve signed protocol IDs without weakening epochs."""
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest
from test_mobile_command_keyboard import method
ROOT = Path(__file__).resolve().parents[1]


def program():
    refs = (ROOT / 'crates/horizon-client/src/game/modding/native_entity_refs.rs').read_text()
    data = (ROOT / 'crates/horizon-client/src/game/modding/native_entity_data.rs').read_text()
    methods = '\n'.join(method(refs, name) for name in ('native_entity_target', 'native_client_entity_target', 'native_entity_read_target', 'native_client_tracked', 'entity', 'entity_position', 'entity_kind', 'entity_uuid'))
    methods += '\n' + method(data, 'entity_name')
    commands=(ROOT / 'crates/horizon-client/src/game/administration/commands/mod.rs').read_text()
    methods += '\n' + method(commands, 'level_entity')
    read=(ROOT/'crates/horizon-client/src/game/modding/native_entity_refs/tick_item_read.rs').read_text()
    methods += '\n'+method(read, 'unscoped_native_entity_target')+'\n'+method(read, 'scoped_native_entity_target')
    return STUBS + method(refs, 'uuid_from_words') + '\nimpl Game {' + methods + '}\n' + CASES


STUBS = r'''
#![allow(dead_code)]
use std::collections::{HashMap,HashSet};
extern crate self as horizon_client;
mod mod_api{pub type EntityTypeKey=String;pub mod __private{pub fn resolved_entity_type_key(name:&str)->Option<String>{Some(name.into())}}}
mod join_seq{pub mod residency{#[derive(Clone,Copy)]pub enum Kind{Player,Hook,DragonPart,Mob}pub struct Owner{pub dimension:u32,pub kind:Kind}pub struct Location{pub owner:Owner}pub mod table{pub enum EntityLookup{Found{location:super::Location,value:crate::Target},Missing}}}}
struct Dim;impl Dim{fn key(&self)->u32{0}}
struct EntitySections{write_position:Option<(u32,[f64;3])>}

#[derive(Clone,Copy)]struct EntityRef{epoch:u64,id:u32}impl EntityRef{fn new(epoch:u64,id:u32)->Self{Self{epoch,id}}fn epoch(self)->u64{self.epoch}fn id(self)->u32{self.id}}
#[derive(Clone,Copy)]enum Target{Knot(u32),Player,Guest(u32),Mob(u32),Item(u32),Orb(u32),Projectile(u32),Stand(u32),Cushion(u32),Frame(u32),Painting(u32),Boat(u32),Cart(u32),Other(u32),Fangs(u32),Crystal(u32),Dragon(u32),Tnt(u32),Falling(u32),Rocket(u32),Trident(u32),Spawner(u32),Cloud(u32)}
struct Lifecycle{entity_sections:EntitySections}impl Lifecycle{fn epoch(&self)->Option<u64>{Some(1)}}
struct Guest{placed:bool,entity_id:i32,conn:u32,uuid:u128}struct Hub{guests:Vec<Guest>}struct Lan{hub:Hub}
struct Mob{id:u32,removed:bool}struct Name(String);impl Name{fn plain(&self)->String{self.0.clone()}}
struct Stand{id:u32,custom_name:Option<Name>}
struct Remote{logged_in:bool,entity_id:i32,rows:HashMap<i32,(&'static str,[f64;3],u128,&'static str)>}
impl Remote{fn tracked(&self,id:i32)->Option<()>{self.rows.contains_key(&id).then_some(())}fn native_epoch(&self)->Option<u64>{Some(1)}fn tracked_entity(&self,id:i32)->Option<(&str,[f64;3])>{self.rows.get(&id).map(|r|(r.0,r.1))}fn level_player(&self,id:i32)->Option<(u128,Option<&str>)>{self.rows.get(&id).filter(|r|r.0=="minecraft:player").map(|r|(r.2,Some(r.3)))}}
struct Persist{uuids:HashMap<u32,[i32;4]>}
struct Game{dim:Dim,held_out:HashSet<u32>,block_entity_tick_items:Option<()>,remote:Option<Remote>,native_lifecycle:Lifecycle,player:Option<()>,lan:Vec<Lan>,lan_acting:Option<u32>,mobs:Vec<Mob>,armor_stands:Vec<Stand>,persist:Persist}
impl Game{
 fn new()->Self{Self{dim:Dim,held_out:HashSet::new(),block_entity_tick_items:None,remote:Some(Remote{logged_in:true,entity_id:1,rows:HashMap::new()}),native_lifecycle:Lifecycle{entity_sections:EntitySections{write_position:None}},player:Some(()),lan:vec![],lan_acting:None,mobs:vec![],armor_stands:vec![],persist:Persist{uuids:HashMap::new()}}}
 fn is_logical_server(&self)->bool{self.remote.is_none()}
 fn tick_item_view(&self,_:u32)->Option<Option<()>>{None}
 fn native_level_key(&self)->u32{0}
 fn current_player_resident_location(&self)->Option<join_seq::residency::Location>{None}
 fn known_resident_entity_uuid_at(&self,_:u32,_:u32)->Option<[i32;4]>{None}
 fn level_dragon_part(&self,_:u32)->Option<Target>{None}
 fn native_entity_kind_key(&self,target:Target)->Option<String>{Some(self.target_type(target))}
 fn server_resident_entity(&self,id:u32)->join_seq::residency::table::EntityLookup{
  if self.remote.as_ref().is_some_and(|r|r.rows.contains_key(&(id as i32))){join_seq::residency::table::EntityLookup::Found{location:join_seq::residency::Location{owner:join_seq::residency::Owner{dimension:0,kind:join_seq::residency::Kind::Mob}},value:Target::Mob(id)}}else{join_seq::residency::table::EntityLookup::Missing}
 }
 fn level_holds(&self,t:Target)->bool{matches!(t,Target::Mob(id)if self.mobs.iter().any(|m|m.id==id))}
 fn target_position(&self,t:Target)->Option<[f64;3]>{if let Target::Mob(id)=t{self.remote.as_ref()?.rows.get(&(id as i32)).map(|r|r.1)}else{Some([0.;3])}}
 fn target_type(&self,t:Target)->String{if let Target::Mob(id)=t{self.remote.as_ref().unwrap().rows[&(id as i32)].0.into()}else{"minecraft:player".into()}}
 fn target_name(&self,_:Target)->Name{Name("fallback".into())}
 fn command_guest(&self,conn:u32)->Option<&Guest>{self.lan.iter().flat_map(|l|&l.hub.guests).find(|g|g.conn==conn)}
 fn uuid_for(&mut self,id:u32)->[i32;4]{self.persist.uuids.get(&id).copied().unwrap_or([0;4])}
}
'''
CASES = r'''
fn main(){let mut g=Game::new();for id in [7,-1,i32::MIN]{
 let key=id as u32;g.remote.as_mut().unwrap().rows.insert(id,("minecraft:player",[2.,64.,3.],123,"NPC"));g.mobs.push(Mob{id:key,removed:false});
 let entity=g.entity(key).expect("a tracked signed-ID player needs a native handle");assert_eq!(entity.id(),key);assert_eq!(g.entity_position(entity),Some([2.,64.,3.]));assert_eq!(g.entity_kind(entity).as_deref(),Some("minecraft:player"));assert_eq!(g.entity_uuid(entity),Some(123));assert_eq!(g.entity_name(entity).as_deref(),Some("NPC"));
 let stale=EntityRef::new(2,key);
 assert!(g.entity_kind(stale).is_none());assert!(g.entity_position(stale).is_none());assert!(g.entity_uuid(stale).is_none());assert!(g.entity_name(stale).is_none());
 g.remote.as_mut().unwrap().logged_in=false;
 assert!(g.entity(key).is_none(),"configuration must not expose a former client level");
 g.remote.as_mut().unwrap().logged_in=true;
 }
 // Tracked-only display entities use the same signed wire ID space.
 let id=-123i32;g.remote.as_mut().unwrap().rows.insert(id,("minecraft:text_display",[4.,5.,6.],456,"Label"));g.persist.uuids.insert(id as u32,[0,0,0,456]);let e=g.entity(id as u32).unwrap();assert_eq!(g.entity_position(e),Some([4.,5.,6.]));assert_eq!(g.entity_uuid(e),Some(456));
 g.remote=None;assert!(g.entity(u32::MAX).is_none(),"server ID validation stays unchanged");
}
'''


class NativeClientSignedIdTests(unittest.TestCase):
    def test_signed_protocol_ids_and_stale_epoch_guards(self):
        if not shutil.which('rustc'):
            self.skipTest('rustc is required')
        with tempfile.TemporaryDirectory(prefix='horizon-native-ids-') as folder:
            src=Path(folder)/'probe.rs';binary=Path(folder)/'probe';src.write_text(program())
            built=subprocess.run(['rustc','--edition=2024',str(src),'-o',str(binary)],capture_output=True,text=True,timeout=60)
            self.assertEqual(built.returncode,0,built.stdout+built.stderr)
            run=subprocess.run([str(binary)],capture_output=True,text=True,timeout=10)
            self.assertEqual(run.returncode,0,run.stdout+run.stderr)
