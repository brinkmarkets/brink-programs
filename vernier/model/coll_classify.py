import sys, math
DEF=[120,230,330,600]
cnt={'nondefault':0,'precision':0,'other':0,'exact':0,'cap_precision':0,'cap_other':0,'cap_exact':0}
for line in sys.stdin:
    a,b=line.split('|'); a=a.split(); b=b.split()
    if b[0]!='0': continue
    t=int(a[2]); n=int(a[4]); tvl=int(a[5]); up=int(a[6]); ur=int(a[7])
    coll=[int(x) for x in a[22:26]]
    rust_c=int(b[9]); rust_cap=int(b[10])
    ts_c=math.ceil(float(n)*float(DEF[t])/10000.0)
    if ts_c==rust_c: cnt['exact']+=1
    elif coll!=DEF: cnt['nondefault']+=1
    elif n*DEF[t] >= 2**53 or n >= 2**53: cnt['precision']+=1
    else: cnt['other']+=1; print('OTHER',line.strip())
    room=min(max(4800-(up if a[3]=='0' else ur),0), max(8000-up-ur,0))
    ts_cap=math.floor(float(room)*float(tvl)/10000.0)
    if ts_cap==rust_cap: cnt['cap_exact']+=1
    elif room*tvl>=2**53 or tvl>=2**53: cnt['cap_precision']+=1
    else: cnt['cap_other']+=1; print('CAPOTHER',line.strip())
print(cnt)
