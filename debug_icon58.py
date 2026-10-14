import ctypes, ctypes.wintypes as wt, json, os, time
import verify_env; verify_env.isolate()
import verify_common as V
u=V.u; u.GetDpiForWindow.restype=ctypes.c_uint; u.GetDpiForWindow.argtypes=[wt.HWND]
CFG=os.path.join(os.environ.get("APPDATA",""),"ZenFile"); USER=os.path.join(CFG,"settings.user.json")
OS_X,OS_Y=60,60
BASE=os.path.join(os.environ.get("TEMP",r"C:\Windows\Temp"),"ZenR57")
ONE=os.path.join(BASE,"one"); os.makedirs(ONE,exist_ok=True); open(os.path.join(ONE,"only.txt"),"w").write("x")
try:
    if os.path.exists(USER): os.remove(USER)
except OSError: pass
d={}; d.setdefault("files",{})["view"]={"default_mode":"large"}; open(USER,"w").write(json.dumps(d))
hw=V.launch(ONE)
dpi=u.GetDpiForWindow(hw) or 96; s=dpi/96.0
r=wt.RECT(); u.GetWindowRect(hw,ctypes.byref(r)); W,H=r.right-r.left,r.bottom-r.top
side=round(157*s); vt=round(34*s)+round(40*s)
print("s",s,"W,H",W,H,"side",side,"vt",vt)
x0=side+round(12*s); y0=vt
V.move(OS_X+W-40, OS_Y+H-round(23*s)-30); time.sleep(0.5)
raw,stride,_,_=V.grab(hw)
# find icon bbox
xs=[]; ys=[]
for y in range(round(y0), round(y0+110*s)):
    for x in range(round(x0)-round(20*s), round(x0+180*s)):
        p=V.px(raw,stride,OS_X+x,OS_Y+y)
        if min(p)>=0x70: xs.append(x); ys.append(y)
print("icon bbox client:", (min(xs),min(ys),max(xs),max(ys)) if xs else None)
bx0,by0,bx1,by1=(min(xs),min(ys),max(xs),max(ys))
cx,cy=((bx0+bx1)//2,(by0+by1)//2)
def ink():
    raw,stride,_,_=V.grab(hw); n=0
    for y in range(by0-8,by1+8):
        for x in range(bx0-8,bx1+8):
            p=V.px(raw,stride,OS_X+x,OS_Y+y)
            if min(p)>=0x70: n+=1
    return n
print("base ink", ink())
# hover
V.move(OS_X+cx, OS_Y+cy); time.sleep(0.7)
print("hover ink", ink())
# back
V.move(OS_X+W-40, OS_Y+H-round(23*s)-30); time.sleep(0.7)
print("after-move ink", ink())
# select (click)
V.move(OS_X+cx, OS_Y+cy); time.sleep(0.05); V.click(OS_X+cx, OS_Y+cy); time.sleep(0.5)
print("selected ink", ink())
# deselect click blank
dx=OS_X+side+round(200*s); dy=OS_Y+(H-round(23*s)-round(20*s))
print("deselect click at", dx, dy)
V.move(dx,dy); time.sleep(0.05); V.click(dx,dy); time.sleep(0.7)
print("after-deselect ink", ink())
sp.run(["taskkill","/F","/IM","ZenFile.exe"],capture_output=True)
