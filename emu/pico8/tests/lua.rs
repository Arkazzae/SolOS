//! P8 Lua as cartridges use it: each test runs a little cartridge and reads
//! its globals back.

use pico8::Pico8;

fn cart(code: &str) -> Vec<u8> {
    format!("pico-8 cartridge // http://www.pico-8.com\nversion 42\n__lua__\n{code}\n").into_bytes()
}

/// Runs code (its top level and _init), and gives the console.
fn run(code: &str) -> Pico8 {
    let mut p = Pico8::new(&cart(code)).unwrap_or_else(|e| panic!("{e}"));
    p.run_frame([0; 8]).unwrap_or_else(|e| panic!("{e}"));
    p
}

fn num(p: &Pico8, name: &str) -> f64 {
    p.global_number(name).unwrap_or_else(|| panic!("{name} = {}", p.global_text(name)))
}

#[test]
fn numbers_are_16_16_fixed_point() {
    let p = run("a=32767+1 b=1/3 c=-7\\2 d=-5%3 e=0x10.8 f=0b11 g=5^2 h=1/0 i=tostr(1/3) j=tostr(-1.5,true)");
    assert_eq!(num(&p, "a"), -32768.0);
    assert_eq!(num(&p, "b"), 0x5555 as f64 / 65536.0);
    assert_eq!(num(&p, "c"), -4.0);
    assert_eq!(num(&p, "d"), 1.0);
    assert_eq!(num(&p, "e"), 16.5);
    assert_eq!(num(&p, "f"), 3.0);
    assert_eq!(num(&p, "g"), 25.0);
    assert!(num(&p, "h") > 32767.0);
    assert_eq!(p.global_text("i"), "0.3333");
    assert_eq!(p.global_text("j"), "0xfffe.8000");
}

#[test]
fn bit_operators_and_peek_operators() {
    let p = run("a=0x0f|0xf0 b=0xff&0x0f c=5^^3 d=1<<4 e=-16>>2 f=-1>>>28 g=~0 poke(0x8000,7,1) h=@0x8000 i=%0x8000 poke4(0x8004,1.5) k=$0x8004");
    assert_eq!(num(&p, "a"), 255.0);
    assert_eq!(num(&p, "b"), 15.0);
    assert_eq!(num(&p, "c"), 6.0);
    assert_eq!(num(&p, "d"), 16.0);
    assert_eq!(num(&p, "e"), -4.0);
    // -1 is 0xffff.0000; shifted right 28 its bits are 0x0000.000f.
    assert_eq!(num(&p, "f"), 15.0 / 65536.0);
    assert_eq!(num(&p, "g"), -1.0 / 65536.0);
    assert_eq!(num(&p, "h"), 7.0);
    assert_eq!(num(&p, "i"), 263.0);
    assert_eq!(num(&p, "k"), 1.5);
}

#[test]
fn closures_share_captured_locals_and_loops_make_fresh_ones() {
    let p = run(
        "function counter() local n=0 return function() n+=1 return n end end
         c=counter() c() c() a=c()
         fs={} for i=1,3 do add(fs,function() return i end) end b=fs[1]()+fs[3]()
         local x=1 local function get() return x end x=5 d=get()
         function fib(n) if n<2 then return n end return fib(n-1)+fib(n-2) end e=fib(15)",
    );
    assert_eq!(num(&p, "a"), 3.0);
    assert_eq!(num(&p, "b"), 4.0);
    assert_eq!(num(&p, "d"), 5.0);
    assert_eq!(num(&p, "e"), 610.0);
}

#[test]
fn metatables_make_classes() {
    let p = run(
        "v={} v.__index=v
         v.__add=function(a,b) return setmetatable({x=a.x+b.x},v) end
         v.__eq=function(a,b) return a.x==b.x end
         v.__lt=function(a,b) return a.x<b.x end
         v.__len=function() return 42 end
         v.__call=function(self,k) return self.x*k end
         v.__concat=function(a,b) return 'cat' end
         v.__unm=function(a) return setmetatable({x=-a.x},v) end
         function v:double() return self.x*2 end
         a=setmetatable({x=2},v) b=setmetatable({x=3},v)
         s=(a+b).x eq=(a==setmetatable({x=2},v)) lt=a<b n=#a c=a(10) m=a:double() cc=a..b u=(-a).x
         d=setmetatable({},{__index=function(t,k) return k..'!' end}) di=d.hi
         log={} w=setmetatable({},{__newindex=function(t,k,v) add(log,k) rawset(t,k,v) end}) w.q=1 w.q=2
         nl=#log",
    );
    assert_eq!(num(&p, "s"), 5.0);
    assert_eq!(p.global_text("eq"), "true");
    assert_eq!(p.global_text("lt"), "true");
    assert_eq!(num(&p, "n"), 42.0);
    assert_eq!(num(&p, "c"), 20.0);
    assert_eq!(num(&p, "m"), 4.0);
    assert_eq!(p.global_text("cc"), "cat");
    assert_eq!(num(&p, "u"), -2.0);
    assert_eq!(p.global_text("di"), "hi!");
    assert_eq!(num(&p, "nl"), 1.0);
}

#[test]
fn coroutines_yield_and_resume_with_values() {
    let p = run(
        "co=cocreate(function(a,b) local c=yield(a+b) local d,e=yield(c*2) return d+e end)
         _,r1=coresume(co,1,2) _,r2=coresume(co,10) _,r3=coresume(co,3,4) s=costatus(co)
         ok,err=coresume(co)
         bad=cocreate(function() local x=nil return x.y end) okb,eb=coresume(bad)
         gen=cocreate(function() for i=1,3 do yield(i) end end) t=0
         while costatus(gen)!='dead' do local _,v=coresume(gen) t+=v or 0 end",
    );
    assert_eq!(num(&p, "r1"), 3.0);
    assert_eq!(num(&p, "r2"), 20.0);
    assert_eq!(num(&p, "r3"), 7.0);
    assert_eq!(p.global_text("s"), "dead");
    assert_eq!(p.global_text("ok"), "false");
    assert_eq!(p.global_text("okb"), "false");
    assert!(p.global_text("eb").contains("index"), "{}", p.global_text("eb"));
    assert_eq!(num(&p, "t"), 6.0);
}

#[test]
fn varargs_select_pack_and_multiple_results() {
    let p = run(
        "function f(...) return select('#',...),... end
         n,a,b=f(5,nil,7) function g() return 1,2,3 end
         t={g()} u={g(),g()} p=pack(g()) l=#t+#u*10+p.n*100
         function h(...) local x,y=... return y end y=h(1,2)
         s=select(2,'a','b','c') m=max(unpack({3,9,4}))",
    );
    assert_eq!(num(&p, "n"), 3.0);
    assert_eq!(num(&p, "a"), 5.0);
    assert_eq!(p.global_text("b"), "[nil]");
    assert_eq!(num(&p, "l"), 3.0 + 40.0 + 300.0);
    assert_eq!(num(&p, "y"), 2.0);
    assert_eq!(p.global_text("s"), "b");
    assert_eq!(num(&p, "m"), 9.0);
}

#[test]
fn tables_lists_and_iteration() {
    let p = run(
        "t={} for i=1,5 do add(t,i*10) end del(t,30) deli(t,1) add(t,5,1) s='' for v in all(t) do s..=v..',' end
         k={x=1,y=2,z=3} n=0 for key,v in pairs(k) do n+=v end
         i=0 for _,v in ipairs({1,2,nil,4}) do i+=1 end
         d={1,2,3,4} for v in all(d) do if v%2==0 then del(d,v) end end
         c=count(d) e=#split('a,b,,c') f=split('1,2,x')[1]+1
         h={1,nil,3} lh=select('#',unpack(h)) q={} q[1]=1 q[2]=2 q[4]=4 q[3]=3 lq=#q",
    );
    assert_eq!(p.global_text("s"), "5,20,40,50,");
    assert_eq!(num(&p, "n"), 6.0);
    assert_eq!(num(&p, "i"), 2.0);
    assert_eq!(num(&p, "c"), 2.0);
    assert_eq!(num(&p, "e"), 4.0);
    assert_eq!(num(&p, "f"), 2.0);
    assert_eq!(num(&p, "lh"), 3.0);
    assert_eq!(num(&p, "lq"), 4.0);
}

#[test]
fn strings_are_bytes() {
    let p = run(
        "s='héllo' a=sub('hello',2,3) b=sub('hello',-3) c=ord('a') d=chr(104,105) e=#'\\^w' f=tonum('0x1f')
         g=tostr(nil) h=tonum('nope') i=('abc')[2]",
    );
    assert_eq!(p.global_text("a"), "el");
    assert_eq!(p.global_text("b"), "llo");
    assert_eq!(num(&p, "c"), 97.0);
    assert_eq!(p.global_text("d"), "hi");
    assert_eq!(num(&p, "e"), 2.0);
    assert_eq!(num(&p, "f"), 31.0);
    assert_eq!(p.global_text("g"), "[nil]");
    assert_eq!(p.global_text("h"), "[nil]");
    assert_eq!(p.global_text("i"), "b");
}

#[test]
fn pico8_syntax_and_goto() {
    let p = run(
        "a=0 if (a==0) a=1 b=2
         c=0 while (c<5) c+=1
         d=0 for i=1,10 do if i%2==0 then goto continue end d+=i ::continue:: end
         e=0 ::top:: e+=1 if e<3 then goto top end
         f = 1 != 2 m,m=1,2 n,o,n=3,4
         g=0 repeat local h=g g+=1 until h>=3",
    );
    assert_eq!(num(&p, "a"), 1.0);
    assert_eq!(num(&p, "b"), 2.0);
    assert_eq!(num(&p, "c"), 5.0);
    assert_eq!(num(&p, "d"), 25.0);
    assert_eq!(num(&p, "e"), 3.0);
    assert_eq!(p.global_text("f"), "true");
    assert_eq!(num(&p, "m"), 1.0);
    assert_eq!(num(&p, "n"), 3.0);
    assert_eq!(num(&p, "g"), 4.0);
}

#[test]
fn env_locals_keep_the_api_in_reach() {
    let p = run(
        "function f(_ENV) return x+flr(y) end r=f({x=1,y=2.5})
         function g() local _ENV=setmetatable({},{__index=_ENV}) z=7 return z end s=g() t=z==nil",
    );
    assert_eq!(num(&p, "r"), 3.0);
    assert_eq!(num(&p, "s"), 7.0);
    assert_eq!(p.global_text("t"), "true");
}

#[test]
fn errors_name_what_was_nil_and_the_line() {
    let mut p = Pico8::new(&cart("x=1\nfunction _update() missing() end")).unwrap();
    let e = p.run_frame([0; 8]).unwrap_err().to_string();
    assert!(e.contains("line 2") && e.contains("global 'missing'"), "{e}");
    assert_eq!(p.run_frame([0; 8]).unwrap_err().to_string(), e);
    assert!(Pico8::new(&cart("x=")).err().unwrap().to_string().contains("syntax error"));
}

#[test]
fn the_collector_frees_cycles_and_keeps_what_is_reachable() {
    let mut p = Pico8::new(&cart(
        "keep={} function _update() for i=1,2000 do local a={} local b={a} a[1]=b end keep[#keep%10+1]={n=#keep} end",
    ))
    .unwrap();
    for _ in 0..200 {
        p.run_frame([0; 8]).unwrap();
    }
    assert_eq!(p.global_text("keep").as_str(), "[table]");
    let mem = Pico8::new(&cart("function _update() m=stat(0) end")).unwrap();
    drop(mem);
}

#[test]
fn flip_shows_frames_from_a_loop_of_the_cartridge_own() {
    let mut p = Pico8::new(&cart("n=0 while true do n+=1 cls(n%16) flip() end")).unwrap();
    for _ in 0..5 {
        p.run_frame([0; 8]).unwrap();
    }
    assert_eq!(p.global_number("n"), Some(5.0));
    assert_eq!(p.frame()[0], 5);
}

#[test]
fn intrinsics_act_as_their_natives_until_redefined() {
    let p = run(
        "a=flr(3.7) b=min(5,2) c=mid(3,1,2) d=flr('2.5') n=flr(-0.5)
         poke(0x8000,300) f=peek(0x8000) pset(1,1,9) g=pget(1,1) fset(1,0x81) h=fget(1) h2=fget(1,7) h3=fget(1,nil)
         i=sgn(-3) j=band(6,3) k=sin(0.25) l=cos(0) m=abs(-32768)
         function flr(x) return 99 end e=flr(1.5)
         local function min(a,b) return 'local' end o=min(1,2)",
    );
    assert_eq!(num(&p, "a"), 3.0);
    assert_eq!(num(&p, "b"), 2.0);
    assert_eq!(num(&p, "c"), 2.0);
    assert_eq!(num(&p, "d"), 2.0);
    assert_eq!(num(&p, "n"), -1.0);
    assert_eq!(num(&p, "f"), 44.0);
    assert_eq!(num(&p, "g"), 9.0);
    assert_eq!(num(&p, "h"), 129.0);
    assert_eq!(p.global_text("h2"), "true");
    assert_eq!(p.global_text("h3"), "true");
    assert_eq!(num(&p, "i"), -1.0);
    assert_eq!(num(&p, "j"), 2.0);
    assert_eq!(num(&p, "k"), -1.0);
    assert_eq!(num(&p, "l"), 1.0);
    assert_eq!(num(&p, "m"), -32768.0);
    assert_eq!(num(&p, "e"), 99.0);
    assert_eq!(p.global_text("o"), "local");
}

#[test]
fn random_numbers_follow_pico8s_generator_rules() {
    let p = run(
        "srand(5) a=rnd() srand(5) z=rnd(0) b=rnd()
         t={10,20,30} srand(3) x=rnd(t) srand(3) y=t[flr(rnd(#t))+1]
         srand(-1) c=rnd() srand(32767) d=rnd()
         srand(9) e=rnd(10) srand(9) f=rnd(10)",
    );
    assert_eq!(num(&p, "z"), 0.0);
    assert_eq!(num(&p, "a"), num(&p, "b"));
    assert_eq!(num(&p, "x"), num(&p, "y"));
    assert_eq!(num(&p, "c"), num(&p, "d"));
    assert_eq!(num(&p, "e"), num(&p, "f"));
    assert!((0.0..10.0).contains(&num(&p, "e")));
}

#[test]
fn deli_keeps_its_index_within_the_list() {
    let p = run("t={1,2,3} a=deli(t,99) b=deli(t,0) n=#t c=t[1]");
    assert_eq!((num(&p, "a"), num(&p, "b"), num(&p, "n"), num(&p, "c")), (3.0, 1.0, 1.0, 2.0));
}
