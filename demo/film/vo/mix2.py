import subprocess,sys
T=float(sys.argv[1])
plan=[("l1.mp3",0.4),("lb.mp3",5.2),("l2.mp3",9.7),("l3.mp3",17.3),("l4.mp3",22.3),("l5.mp3",28.0),("l6.mp3",35.0),("l7.mp3",43.5),("l8.mp3",T-6.2),("l9.mp3",T-3.25)]
ins=sum([["-i",f] for f,_ in plan],[])
fc="".join(f"[{i}:a]adelay={int(t*1000)}|{int(t*1000)}[a{i}];" for i,(_,t) in enumerate(plan))
fc+="".join(f"[a{i}]" for i in range(len(plan)))+f"amix=inputs={len(plan)}:normalize=0,apad,atrim=0:{T},loudnorm=I=-16:TP=-1.5[out]"
subprocess.check_call(["ffmpeg","-v","error","-y",*ins,"-filter_complex",fc,"-map","[out]","-ar","48000","vo2.wav"])
