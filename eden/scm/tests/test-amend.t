#testcases obsstore-off obsstore-on mutation
  $ enable amend
  $ setconfig diff.git=1
  $ configure evolution
#endif
#if obsstore-off
  $ configure noevolution
#endif
#if mutation
  $ configure mutation-norecord
#endif
#if mutation

  $ hg log -T '{rev} {node|short} {desc}\n' -G
  @  3 be169c7e8dbe B
  |
  | o  2 26805aba1e60 C
  | |
  | x  1 112478962961 B
  |/
  o  0 426bada5c675 A
  