#!/usr/bin/env python3
"""Capture deterministic GraphQL responses from the pinned Ownfoil test fixture.

Run with the reference repository's requirements and pytest installed. No keys,
commercial content, running server, or live TitleDB downloads are needed.
"""
import argparse
import json
import os
from pathlib import Path
import sys
import tempfile

PIN = "0cce4bbc684b30930b1576847c8c8fb5202114bf"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("upstream", type=Path)
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    upstream, output = args.upstream.resolve(), args.output.resolve()
    with tempfile.TemporaryDirectory(prefix="ownfoil-gql-capture-") as directory:
        root = Path(directory)
        os.environ["OWNFOIL_DATA_DIR"] = str(root / "data")
        sys.path.insert(0, str(upstream / "tests"))
        import conftest  # Establish upstream's isolated imports/config before app imports.
        import pytest
        import test_gql_graph as reference
        from gql.context import GraphQLContext
        from gql.schema import schema

        with pytest.MonkeyPatch.context() as patch:
            library = reference.library.__wrapped__(root, patch)
            title = reference.ALPHA
            queries = [
                '{ titles { items { apps { title { titleId } titledb { apps { id } availableVersions { version } availableDlc { appId } } files { library { id } apps { title { titleId } titledb { name } versions { version } } } } } } }',
                '{ tasks { children { children { id } } } }',

                '{ titles { total items { titleId name ownership { haveBase complete upToDate } } } }',
                '{ titles(owned: true) { total items { titleId name apps { id appId appVersion owned } } } }',
                '{ titles(owned: false) { total items { titleId name } } }',
                '{ apps { total items { id appId appType appVersion owned title { titleId name } titledb { name } versions { version owned } } } }',
                '{ apps(groupByAppId: true) { total items { id appId appVersion owned versions { version owned } } } }',
                '{ files { total items { id filename size libraryId library { id path } apps { id appId title { name } versions { version owned } } } } }',
                '{ apps(appType: [BASE]) { items { files { filename apps { appId files { filename } } } } } }',
                '{ files { items { apps { title { apps { id } } } } } }',
                '{ libraries { id path } tasks { id taskName status input children { id taskName status completionPct } } }',
                '{ tasks(includeChildren: true, status: RUNNING) { id parentId taskName status } task(id:"1") { id children { id } } }',
                '{ app(id:"2") { appId title { name } files { filename } versions { version owned } } file(id:"2") { filename apps { appId } } }',
                '{ app(id:"999") { id } file(id:"999") { id } title(titleId:"0000000000000000") { titleId } task(id:"999") { id } }',
                '{ stats { totalFiles totalSize identifiedFiles unidentifiedFiles totalTitles ownedTitles totalApps ownedApps completeTitles upToDateTitles appsByType { key count owned } filesByExtension { key count size } filesByVerificationStatus { status count size } } }',
                '{ title(titleId:"' + title.lower() + '") { titleId name apps(appType:[BASE]) { appId versions { version owned } } } }',
            ]
            for grouped in ("false", "true"):
                for owned in ("false", "true"):
                    queries += [
                        '{ apps(groupByAppId:' + grouped + ', owned:' + owned + ') { total items { id appId appVersion owned } } }',
                        '{ apps(groupByAppId:' + grouped + ', filter:{owned:' + owned + ', appVersion:{gte:65536}}) { total items { id appId appVersion owned } } }',
                    ]
            for kind, fields in (("apps", "id appId appVersion"), ("files", "id filename size"), ("titles", "titleId name")):
                for field in ("NAME", "SIZE", "VERSION"):
                    for direction in ("ASC", "DESC"):
                        queries.append('{ ' + kind + '(orderBy:{field:' + field + ',direction:' + direction + '},page:1,pageSize:2) { total items { ' + fields + ' } } }')
            cases = []
            with library.app.app_context():
                for admin, shop in ((True, True), (False, True), (True, False)):
                    for query in queries:
                        result = schema.execute_sync(query, context_value=GraphQLContext(None, admin, shop))
                        if result.errors:
                            raise RuntimeError(str(result.errors))
                        # Paths are irrelevant to the comparison; normalize only the fixture root.
                        data = json.loads(json.dumps(result.data).replace(str(root), "/parity"))
                        cases.append({"admin": admin, "shop": shop, "query": query, "data": data})
            fixture = {"upstream": PIN, "title": title, "metadata": reference.TITLEDB_JSON,
                       "apps": reference.APPS, "cases": cases}
            output.parent.mkdir(parents=True, exist_ok=True)
            output.write_text(json.dumps(fixture, indent=2) + "\n")
            print(f"Captured {len(cases)} GraphQL cases to {output}")


if __name__ == "__main__":
    main()
