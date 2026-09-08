# gitomic

`gitomic` is a light-weight daemon that can be attached to a git repository and track repo changes, creating atomic commmits as you go.
Since git already tracks file changes, gitomic should be able to simply copy git's homework and as file ar changed, create individual commits 
for each change. Each change is diffrent than "each file" as if a file is changed, and then later another edit is made to it, therre having been other repo changes between the first change and the current change those should not be flattened into a single commit.  Each "event" should be recoverable. 

Commits should be created with a blank message, then at the end when the user actually goes to review / write the commit, one message gets entered and that  message gets duplicated across all the commits in the batch . 

gitomic should run as a user-level daemon and be configured via `~/.config/gitomic/gitomic.cfg` the most important options that I have in mind at this point is the ability to add multiple directories as "locations containing git repos," and maybe some basic formatting things... and whateer else would be importnat to such a program when interactnig with git. 

I dont' want gitomic to deal with any credentials, that should all be on the `git` side of things.  Just as narrowly scoped as possible to achieve the goal: atomic commits without having to create individual commits s you go. 

My familiarity with git at this point is still a learning process and discovery of new frutrations, so this nis not a hard definition of the scope. What ever is useful, you may add. 