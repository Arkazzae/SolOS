pico-8 cartridge // http://www.pico-8.com
version 42
__lua__
-- orbit: an original Sol OS input, graphics and sound demo
x=64 y=64 score=0 tx=96 ty=64
function _init()
 cartdata("sol_orbit")
 best=dget(0)
 sfx(0,0,0,4)
end
function _update60()
 if btn(0) then x=max(4,x-1) end
 if btn(1) then x=min(123,x+1) end
 if btn(2) then y=max(24,y-1) end
 if btn(3) then y=min(111,y+1) end
 if abs(x-tx)<6 and abs(y-ty)<6 then
  score+=1
  tx=8+flr(rnd(112)) ty=28+flr(rnd(78))
  sfx(0,0,0,4)
  if score>best then best=score dset(0,best) end
 end
 if btnp(4) or btnp(5) then sfx(0,1,0,4) end
end
function _draw()
 cls(1)
 for i=0,31 do
  pset((i*37)%128,24+(i*17)%88,5)
 end
 print("sol / orbit",4,4,7)
 print("score "..score,4,13,10)
 print("best "..best,82,13,6)
 line(0,21,127,21,5)
 circ(tx,ty,5,10) pset(tx,ty,7)
 circfill(x,y,3,12) pset(x,y-1,7)
 print("arrows: fly   o/x: sound",4,119,6)
end
__sfx__
0004000024070280702b0703007000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000
