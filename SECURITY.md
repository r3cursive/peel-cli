# Security policy

peel runs on untrusted archives by design, so a way to make it write outside its output folder, follow a link it should not, run a binary it should not, or delete something it did not create is a vulnerability.

## Reporting

Please report vulnerabilities privately, through this repository's **Security → Report a vulnerability** form (GitHub private vulnerability reporting). Do not open a public issue for them.

A useful report says what you ran, the archive or tree layout that triggers the problem (described, or attached as a small file), and what happened against what you expected. You will get an acknowledgement, and the fix will be credited to you unless you ask otherwise.

## Scope

In scope: peel's own walking, naming, cleanup, limits and process handling. Problems inside 7-Zip or binwalk themselves belong upstream with those projects, but a way peel's use of them makes things worse is in scope here.
