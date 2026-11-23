# CPZED-S3 manual check (dev Zed, isolated profile)

Run only with a throwaway profile; never touch the real Zed or CPZed.

1. Build/run a dev Zed: `target/debug/zed --user-data-dir "$(mktemp -d)" ~/code/cpzed-sandbox`
   (and a second project folder in the same window, via File > Add Folder).
2. Open two terminal threads in the first project (sidebar "+" > terminal).
3. Right-click a terminal row:
   - "Make Subthread Of > <other row>": the row indents under it with a chevron.
   - "Move to Section > New Section...": type a name, press Enter: a section header
     appears under the project with the row under it.
4. Click the chevron on the parent: it shows "+N" and any running/waiting/notification rollup.
5. Select a parent and press Right/Left: collapsed -> expands; expanded -> first child;
   Left on a child selects the parent; Left on an expanded parent collapses it.
6. Right-click a section header: Rename, Delete Section, Collapse.
7. Quit and relaunch with the same `--user-data-dir`: same nesting and collapse state.
8. With S2's CLI (once merged), build the manager tree without UI:
   `zed --agent-new ... --section Personas`, `--parent <id>`, and confirm the sidebar
   updates live with no click.
9. Type in the sidebar search box: the list goes flat (no sections, no indent).
