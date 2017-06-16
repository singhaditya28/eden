#require test-repo

  $ . $TESTDIR/require-core-hg.sh contrib/check-code.py
  $ . "$TESTDIR/helper-testrepo.sh"
  $ check_code="$RUNTESTDIR"/../contrib/check-code.py

New errors are not allowed. Warnings are strongly discouraged.
(The writing "no-che?k-code" is for not skipping this file when checking.)

  $ hg files "set:grep('Generated by Cython')" > $TESTTMP/cython-generated
  $ echo "${LINTFILES:-`hg locate`}" | sed 's-\\-/-g' | grep -v clib/sha1/ > "$TESTTMP/check-files"
  $ cat "$TESTTMP/cython-generated" | while read i; do
  >     grep -F -v "$i" "$TESTTMP/check-files" > "$TESTTMP/check-files-new"
  >     mv "$TESTTMP/check-files-new" "$TESTTMP/check-files"
  > done
  $ cat "$TESTTMP/check-files" | "$check_code" --warnings --per-file=0 - || false
  Skipping cdatapack/cdatapack.c it has no-che?k-code (glob)
  Skipping cdatapack/cdatapack.h it has no-che?k-code (glob)
  Skipping cdatapack/cdatapack_dump.c it has no-che?k-code (glob)
  Skipping cdatapack/cdatapack_get.c it has no-che?k-code (glob)
  Skipping cfastmanifest.c it has no-che?k-code (glob)
  Skipping cfastmanifest/bsearch.c it has no-che?k-code (glob)
  Skipping cfastmanifest/bsearch.h it has no-che?k-code (glob)
  Skipping cfastmanifest/bsearch_test.c it has no-che?k-code (glob)
  Skipping cfastmanifest/checksum.c it has no-che?k-code (glob)
  Skipping cfastmanifest/checksum.h it has no-che?k-code (glob)
  Skipping cfastmanifest/checksum_test.c it has no-che?k-code (glob)
  Skipping cfastmanifest/internal_result.h it has no-che?k-code (glob)
  Skipping cfastmanifest/node.c it has no-che?k-code (glob)
  Skipping cfastmanifest/node.h it has no-che?k-code (glob)
  Skipping cfastmanifest/node_test.c it has no-che?k-code (glob)
  Skipping cfastmanifest/path_buffer.h it has no-che?k-code (glob)
  Skipping cfastmanifest/result.h it has no-che?k-code (glob)
  Skipping cfastmanifest/tests.c it has no-che?k-code (glob)
  Skipping cfastmanifest/tests.h it has no-che?k-code (glob)
  Skipping cfastmanifest/tree.c it has no-che?k-code (glob)
  Skipping cfastmanifest/tree.h it has no-che?k-code (glob)
  Skipping cfastmanifest/tree_arena.c it has no-che?k-code (glob)
  Skipping cfastmanifest/tree_arena.h it has no-che?k-code (glob)
  Skipping cfastmanifest/tree_convert.c it has no-che?k-code (glob)
  Skipping cfastmanifest/tree_convert_rt.c it has no-che?k-code (glob)
  Skipping cfastmanifest/tree_convert_test.c it has no-che?k-code (glob)
  Skipping cfastmanifest/tree_copy.c it has no-che?k-code (glob)
  Skipping cfastmanifest/tree_copy_test.c it has no-che?k-code (glob)
  Skipping cfastmanifest/tree_diff.c it has no-che?k-code (glob)
  Skipping cfastmanifest/tree_diff_test.c it has no-che?k-code (glob)
  Skipping cfastmanifest/tree_disk.c it has no-che?k-code (glob)
  Skipping cfastmanifest/tree_disk_test.c it has no-che?k-code (glob)
  Skipping cfastmanifest/tree_dump.c it has no-che?k-code (glob)
  Skipping cfastmanifest/tree_iterate_rt.c it has no-che?k-code (glob)
  Skipping cfastmanifest/tree_iterator.c it has no-che?k-code (glob)
  Skipping cfastmanifest/tree_iterator.h it has no-che?k-code (glob)
  Skipping cfastmanifest/tree_iterator_test.c it has no-che?k-code (glob)
  Skipping cfastmanifest/tree_path.c it has no-che?k-code (glob)
  Skipping cfastmanifest/tree_path.h it has no-che?k-code (glob)
  Skipping cfastmanifest/tree_test.c it has no-che?k-code (glob)
  Skipping clib/buffer.c it has no-che?k-code (glob)
  Skipping clib/buffer.h it has no-che?k-code (glob)
  Skipping clib/null_test.c it has no-che?k-code (glob)
  Skipping cstore/datapackstore.cpp it has no-che?k-code (glob)
  Skipping cstore/datapackstore.h it has no-che?k-code (glob)
  Skipping cstore/key.h it has no-che?k-code (glob)
  Skipping cstore/match.h it has no-che?k-code (glob)
  Skipping cstore/py-cdatapack.h it has no-che?k-code (glob)
  Skipping cstore/py-cstore.cpp it has no-che?k-code (glob)
  Skipping cstore/py-datapackstore.h it has no-che?k-code (glob)
  Skipping cstore/py-structs.h it has no-che?k-code (glob)
  Skipping cstore/py-treemanifest.h it has no-che?k-code (glob)
  Skipping cstore/pythonutil.cpp it has no-che?k-code (glob)
  Skipping cstore/pythonutil.h it has no-che?k-code (glob)
  Skipping cstore/store.h it has no-che?k-code (glob)
  Skipping cstore/uniondatapackstore.cpp it has no-che?k-code (glob)
  Skipping cstore/uniondatapackstore.h it has no-che?k-code (glob)
  Skipping ctreemanifest/manifest.cpp it has no-che?k-code (glob)
  Skipping ctreemanifest/manifest.h it has no-che?k-code (glob)
  Skipping ctreemanifest/manifest_entry.cpp it has no-che?k-code (glob)
  Skipping ctreemanifest/manifest_entry.h it has no-che?k-code (glob)
  Skipping ctreemanifest/manifest_fetcher.cpp it has no-che?k-code (glob)
  Skipping ctreemanifest/manifest_fetcher.h it has no-che?k-code (glob)
  Skipping ctreemanifest/treemanifest.cpp it has no-che?k-code (glob)
  Skipping ctreemanifest/treemanifest.h it has no-che?k-code (glob)
  Skipping tests/conduithttp.py it has no-che?k-code (glob)
  tests/test-rage.t:10:
   >   $ echo "rpmbin = /bin/rpm" >> .hg/hgrc
   don't use explicit paths for tools
  Skipping tests/test-remotefilelog-bad-configs.t it has no-che?k-code (glob)
  [1]

Check foreign extensions are only used after checks

  $ hg locate 'test-*.t' | xargs $TESTDIR/check-foreignext.py

No files with home directory committed

  $ hg files "set:grep('/""home/')"
  [1]
