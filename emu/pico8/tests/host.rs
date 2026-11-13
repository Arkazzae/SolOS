//! What a host relies on: frames, sound, saved data, the menu, errors.

use pico8::Pico8;

fn cart(code: &str) -> Vec<u8> {
    format!("pico-8 cartridge // http://www.pico-8.com\nversion 42\n__lua__\n{code}\n").into_bytes()
}

#[test]
fn the_refresh_rate_follows_update60() {
    let mut p = Pico8::new(&cart("function _update60() end function _init() rate=stat(7) end")).unwrap();
    p.run_frame([0; 8]).unwrap();
    assert_eq!(p.fps(), 60);
    assert_eq!(p.global_number("rate"), Some(60.0));
    assert_eq!(p.audio().len(), 367);
    p.run_frame([0; 8]).unwrap();
    assert_eq!(p.audio().len(), 368);
    let mut p = Pico8::new(&cart("function _update() end")).unwrap();
    p.run_frame([0; 8]).unwrap();
    assert_eq!((p.fps(), p.audio().len()), (30, 735));
}

#[test]
fn saved_data_goes_in_before_the_cartridge_starts() {
    let source = cart("loaded=cartdata('host_test') v=dget(0) function _update() dset(1,v+1) end");
    let mut fresh = Pico8::new(&source).unwrap();
    fresh.run_frame([0; 8]).unwrap();
    assert_eq!(fresh.global_text("loaded"), "false");
    assert_eq!(fresh.cartdata_id(), Some("host_test"));
    let mut saved = [0u8; 256];
    saved[..4].copy_from_slice(&(7i32 << 16).to_le_bytes());
    let mut loaded = Pico8::new(&source).unwrap();
    loaded.set_persistent(&saved);
    loaded.run_frame([0; 8]).unwrap();
    assert_eq!(loaded.global_text("loaded"), "true");
    assert_eq!(loaded.persistent()[4..8], (8i32 << 16).to_le_bytes());
}

#[test]
fn buttons_and_btnp_repeat() {
    let mut p = Pico8::new(&cart("n=0 function _update() if btnp(5) then n+=1 end held=btn(5) end")).unwrap();
    for _ in 0..40 {
        p.run_frame([pico8::X, 0, 0, 0, 0, 0, 0, 0]).unwrap();
    }
    assert_eq!(p.global_text("held"), "true");
    // The first press, then frame 16 and every 4 frames after: 20 … 40.
    assert_eq!(p.global_number("n"), Some(1.0 + 7.0));
}

#[test]
fn menu_items_call_back() {
    let mut p = Pico8::new(&cart("hits=0 menuitem(1,'retry',function(b) hits+=1 return true end) function _update() end")).unwrap();
    p.run_frame([0; 8]).unwrap();
    assert_eq!(p.menu_items(), [pico8::MenuItem { index: 1, label: "retry".into() }]);
    assert!(p.activate_menu_item(1, 4).unwrap());
    assert_eq!(p.global_number("hits"), Some(1.0));
}

#[test]
fn sound_plays_and_stops() {
    let mut p = Pico8::new(&cart("poke2(0x3200,0x0e18) poke(0x3241,1) sfx(0) function _update() end")).unwrap();
    p.run_frame([0; 8]).unwrap();
    assert!(p.audio().iter().any(|&s| s != 0));
    for _ in 0..10 {
        p.run_frame([0; 8]).unwrap();
    }
    assert!(p.audio().iter().all(|&s| s == 0));
}

#[test]
fn an_error_stops_the_game_and_shows_on_the_picture() {
    let mut p = Pico8::new(&cart("function _update() local t=nil t.x=1 end")).unwrap();
    let e = p.run_frame([0; 8]).unwrap_err();
    assert!(e.0.starts_with("runtime error line 1"), "{e}");
    assert!(p.frame().contains(&8));
    assert!(p.error().is_some());
}

#[test]
fn a_frame_without_draw_updates_and_keeps_the_picture() {
    let mut p = Pico8::new(&cart("u=0 d=0 function _update60() u+=1 end function _draw() d+=1 cls(u%16) end")).unwrap();
    p.run_frame([0; 8]).unwrap();
    let before = p.frame()[0];
    p.run_frame_without_draw([0; 8]).unwrap();
    assert_eq!((p.global_number("u"), p.global_number("d")), (Some(2.0), Some(1.0)));
    assert_eq!(p.frame()[0], before);
    assert!((367..=368).contains(&p.audio().len()));
    p.run_frame([0; 8]).unwrap();
    assert_eq!((p.global_number("u"), p.global_number("d")), (Some(3.0), Some(2.0)));
}

#[test]
fn a_text_cartridge_label_is_its_cover() {
    let row = format!("{}{}\n", "8".repeat(64), "g".repeat(64));
    let text = format!("{}__label__\n{}\n", String::from_utf8(cart("x=1")).unwrap(), row.repeat(128));
    let label = pico8::label(text.as_bytes()).unwrap();
    assert_eq!((label[0], label[64], label[128 * 127 + 127]), (8, 16, 16));
    assert!(pico8::label(&cart("x=1")).is_none());
}

#[test]
fn stop_ends_the_game_without_an_error() {
    let mut p = Pico8::new(&cart("function _update() n=(n or 0)+1 if n==3 then stop() end end")).unwrap();
    for _ in 0..5 {
        p.run_frame([0; 8]).unwrap();
    }
    assert!(p.ended());
    assert_eq!(p.global_number("n"), Some(3.0));
}

/// Runs frames with changing buttons; the pictures and sound of them all.
fn play(p: &mut Pico8, from: u32, n: u32) -> (Vec<u8>, Vec<i16>) {
    let (mut pic, mut snd) = (Vec::new(), Vec::new());
    for f in from..from + n {
        let b = [(f % 7) as u8 & 0x3f, 0, 0, 0, 0, 0, 0, 0];
        p.run_frame(b).unwrap();
        pic.extend_from_slice(p.frame());
        snd.extend_from_slice(p.audio());
    }
    (pic, snd)
}

#[test]
fn a_saved_state_plays_on_the_same() {
    let code = "
        poke2(0x3200,0x0e18) poke2(0x3202,0x1e20) poke(0x3241,3) poke(0x3243,2)
        ps={} t={}
        co=cocreate(function() for i=1,1000 do t[#t+1]=i yield(i) end end)
        mt={__index=function(s,k) return k*2 end}
        function _update()
          local _,v=coresume(co) add(ps,{x=rnd(128),y=v%128,c=setmetatable({},mt)[v%16]})
          if #ps>40 then deli(ps,1) end
          if btnp(4) then sfx(0) end
        end
        function _draw() cls(1) for p in all(ps) do pset(p.x,p.y,p.c) end print(#t,0,0,7) end";
    let source = cart(code);
    let mut a = Pico8::new(&source).unwrap();
    play(&mut a, 0, 50);
    let state = a.save_state();
    let after = play(&mut a, 50, 60);
    let mut b = Pico8::new(&source).unwrap();
    b.load_state(&state).unwrap();
    assert_eq!(play(&mut b, 50, 60), after);
    // And from the state again, in the same console.
    a.load_state(&state).unwrap();
    assert_eq!(play(&mut a, 50, 60), after);
}

#[test]
fn a_state_stopped_at_a_flip_goes_on_from_there() {
    let source = cart("n=0 while true do for i=1,3 do n+=1 flip() end cls(n%16) end");
    let mut a = Pico8::new(&source).unwrap();
    play(&mut a, 0, 7);
    let state = a.save_state();
    let after = play(&mut a, 7, 9);
    let mut b = Pico8::new(&source).unwrap();
    b.load_state(&state).unwrap();
    assert_eq!(play(&mut b, 7, 9), after);
    assert_eq!(b.global_number("n"), Some(16.0));
}

#[test]
fn a_state_of_another_cartridge_or_damaged_is_refused() {
    let mut a = Pico8::new(&cart("x={1,2,3} function _update() x[1]+=1 end")).unwrap();
    play(&mut a, 0, 3);
    let state = a.save_state();
    let mut other = Pico8::new(&cart("y=1 function _update() end")).unwrap();
    assert!(other.load_state(&state).is_err());
    for cut in [0, 10, state.len() / 2, state.len() - 1] {
        assert!(a.load_state(&state[..cut]).is_err(), "{cut}");
    }
    let mut bad = state.clone();
    for i in (40..bad.len()).step_by(97) {
        bad[i] ^= 0x5a;
    }
    let _ = a.load_state(&bad);
    // Whatever it did, the game still runs or was left as it was.
    a.load_state(&state).unwrap();
    play(&mut a, 3, 3);
}

#[test]
fn the_pause_menu_runs_the_cartridge_items_and_continues() {
    let mut p = Pico8::new(&cart(
        "n=0 hits=0 menuitem(1,'more',function(b) if b&32>0 then hits+=1 end if b&2>0 then hits+=10 end return b&2>0 end)
         function _update() n+=1 end",
    ))
    .unwrap();
    let press = |p: &mut Pico8, b: u8| {
        p.run_frame([b, 0, 0, 0, 0, 0, 0, 0]).unwrap();
        p.run_frame([0; 8]).unwrap();
    };
    press(&mut p, 0);
    let n = p.global_number("n").unwrap();
    // Held for a few frames, as a person presses it: it stays open.
    for _ in 0..4 {
        p.run_frame([pico8::PAUSE, 0, 0, 0, 0, 0, 0, 0]).unwrap();
    }
    p.run_frame([0; 8]).unwrap();
    assert!(p.paused());
    assert!(p.audio().iter().all(|&s| s == 0));
    // The game stands still while it is open.
    assert_eq!(p.global_number("n"), Some(n));
    press(&mut p, pico8::DOWN);
    press(&mut p, pico8::RIGHT);
    assert_eq!(p.global_number("hits"), Some(10.0));
    assert!(p.paused());
    press(&mut p, pico8::O);
    assert_eq!(p.global_number("hits"), Some(11.0));
    assert!(!p.paused());
    press(&mut p, 0);
    assert!(p.global_number("n").unwrap() > n);
}

#[test]
fn a_cartridge_can_keep_the_pause_menu_shut_and_see_the_button() {
    let mut p = Pico8::new(&cart("function _update() poke(0x5f30,1) if btnp(6) then seen=true end end")).unwrap();
    p.run_frame([0; 8]).unwrap();
    p.run_frame([pico8::PAUSE, 0, 0, 0, 0, 0, 0, 0]).unwrap();
    assert!(!p.paused());
    assert_eq!(p.global_text("seen"), "true");
}

#[test]
fn reset_cart_starts_the_game_again_and_keeps_its_save() {
    let mut p = Pico8::new(&cart("cartdata('r') dset(0,dget(0)+1) runs=dget(0) function _update() end")).unwrap();
    p.run_frame([0; 8]).unwrap();
    p.run_frame([pico8::PAUSE, 0, 0, 0, 0, 0, 0, 0]).unwrap();
    for b in [pico8::UP, 0, pico8::O, 0, 0] {
        p.run_frame([b, 0, 0, 0, 0, 0, 0, 0]).unwrap();
    }
    assert_eq!(p.global_number("runs"), Some(2.0));
}


#[test]
fn a_menu_item_renames_itself_from_its_callback() {
    let mut p = Pico8::new(&cart(
        "on=true function toggle(b) on=not on menuitem(nil, on and 'music: on' or 'music: off') return true end
         menuitem(1,'music: on',toggle) function _update() end",
    ))
    .unwrap();
    p.run_frame([0; 8]).unwrap();
    assert!(p.activate_menu_item(1, 32).unwrap());
    assert_eq!(p.menu_items()[0].label, "music: off");
    assert!(p.activate_menu_item(1, 32).unwrap());
    assert_eq!(p.menu_items()[0].label, "music: on");
}

#[test]
fn print_returns_where_the_text_ended_and_scrolls_after_the_last_line() {
    let mut p = Pico8::new(&cart("x,y=print('hi',10,20,7) cursor(0,120) print('a') cy=peek(0x5f27) function _update() end")).unwrap();
    p.run_frame([0; 8]).unwrap();
    assert_eq!((p.global_number("x"), p.global_number("y")), (Some(18.0), Some(26.0)));
    // A line at 120 leaves the next at 126, which does not fit: up 6.
    assert_eq!(p.global_number("cy"), Some(120.0));
}

#[test]
fn a_colour_can_carry_a_fill_pattern() {
    // 0x5f34 on: rectfill with 0x1017.5a5a draws pattern 0x5a5a in 7 and 1;
    // with 0x0100.0000 too, the pattern's other pixels are left alone.
    let mut p = Pico8::new(&cart("poke(0x5f34,1) rectfill(0,0,3,3,0x1017.5a5a) a=pget(0,0) b=pget(1,0) rectfill(0,4,3,7,0x1108.5a5a) c=pget(0,4)+pget(1,4) function _update() end")).unwrap();
    p.run_frame([0; 8]).unwrap();
    let (a, b) = (p.global_number("a").unwrap(), p.global_number("b").unwrap());
    assert!(a != b && [a, b].contains(&7.0) && [a, b].contains(&1.0), "{a} {b}");
    assert_eq!(p.global_number("c"), Some(8.0));
}
