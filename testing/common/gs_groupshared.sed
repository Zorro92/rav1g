# For sed -E: the group memory of a shader as arrays that watch their readers and writers
# (testing/common/group_sched.h). 'groupshared T name[N][M];' becomes 'static GsArray<T, (N), (M)> name;'.
s/^groupshared +([A-Za-z0-9_]+) +([A-Za-z0-9_]+)\[([^]]+)\]\[([^]]+)\]\[([^]]+)\];/static GsArray<\1, (\3), (\4), (\5)> \2;/
s/^groupshared +([A-Za-z0-9_]+) +([A-Za-z0-9_]+)\[([^]]+)\]\[([^]]+)\];/static GsArray<\1, (\3), (\4)> \2;/
s/^groupshared +([A-Za-z0-9_]+) +([A-Za-z0-9_]+)\[([^]]+)\];/static GsArray<\1, (\3)> \2;/
