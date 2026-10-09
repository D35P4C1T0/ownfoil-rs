#!/usr/bin/env python3
"""Capture released Sphaira queries from Ownfoil 2.5.0's real GraphQL schema.

Uses synthetic upstream test data; no keys, commercial files or network needed.
Install upstream requirements and pytest first. This does not overwrite 2.4.1.
"""
import argparse
import datetime
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

PIN = "a9ac7479f7b54cd52731b24947ac0631cb77ba5f"
CLIENT = "338348e74b6d278bc570a9be5373d25197bf6c8d"
SOURCE_SHA256 = {
    "app/tasks.py": "ccd8edfb853929005debfc42f1cbf6c152eb4200f1f950ec3c2376383a4f8356",
    "app/containers/nacp.py": "f89d830af3a84087d51d236cbbad2fba7c44d76140491594781f7dcc81808bd8",
    "app/gql/types.py": "c7d07a2e30ef065e7c8ef2a6421790b9f7aa59ec807dc8c14af8dc909584c490",
    "app/app.py": "6282a6d5f701b28449ae448f26bf5f2e022d51a7f535cacacf664193ab3d7c26",
    "app/db.py": "6daa87eb1fab9337c7027cebd07eeffcfc40d18103342a452c7e2bb87e4034df",
    "app/constants.py": "f2fcbc27a12df3ff9a1fb386d31972191ab337225b5fb7b6c7196989fe207b18",
    "app/media.py": "758c1ecd1ab9b80ba94fe0fbc0eea526432d8fdac89bb177794a84d0a920ec50",
    "app/titledb/schema.py": "3dab14bede8652210b08076f428bcfdf01141a5798c60ad1c138c8b97602b79b",
    "app/gql/resolvers.py": "a63f16ef2b4678d962a39588c10bd2ba80f354341c0b1bd57a487af956917eca",
}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("upstream", type=Path)
    parser.add_argument("output", type=Path)
    parser.add_argument("--client-queries", type=Path,
                        default=Path(__file__).resolve().parents[2] /
                        "ownfoil-rs/tests/fixtures/sphaira_native.json")
    args = parser.parse_args()
    upstream, output = args.upstream.resolve(), args.output.resolve()
    source_files = list(SOURCE_SHA256)
    hashes = {name: hashlib.sha256((upstream / name).read_bytes()).hexdigest()
              for name in source_files}
    if hashes != SOURCE_SHA256:
        raise RuntimeError("Source hashes differ from the pinned 2.5.0 capture")
    if (upstream / ".git").exists():
        revision = subprocess.check_output(
            ["git", "-C", str(upstream), "rev-parse", "HEAD"], text=True).strip()
        if revision != PIN:
            raise RuntimeError(f"Expected {PIN}, got {revision}")
    queries = json.loads(args.client_queries.read_text())["cases"]
    with tempfile.TemporaryDirectory(prefix="ownfoil-250-capture-") as directory:
        root = Path(directory)
        os.environ["OWNFOIL_DATA_DIR"] = str(root / "data")
        sys.path.insert(0, str(upstream / "tests"))
        import conftest  # noqa: F401: establish isolated imports/config
        import pytest
        import test_gql_graph as reference
        import settings
        import titledb
        import media
        from constants import DEFAULT_SETTINGS
        from db import Apps, Files, Libraries, Titles, db
        from gql.context import GraphQLContext
        from gql.schema import schema
        from titledb.schema import SOURCE_PRIORITY
        from containers.nacp import language_for_locale

        with pytest.MonkeyPatch.context() as patch:
            library = reference.library.__wrapped__(root, patch)
            patch.setattr(settings, "_cached_settings", json.loads(json.dumps(DEFAULT_SETTINGS)))
            with library.app.app_context():
                # A second owned update and a still-newer unavailable version.
                newer = Apps.query.filter_by(app_id=reference.ALPHA_UPD,
                                             app_version="131072").one()
                newer.owned = True
                new_file = Files(library_id=1, filepath=str(root / "games/Alpha[v131072].nsp"),
                                 filename="Alpha[v131072].nsp", extension="nsp", size=1500,
                                 identified=True)
                newer.files.append(new_file)
                db.session.add(new_file)
                db.session.add(Apps(title_id=1, app_id=reference.ALPHA_UPD,
                                    app_version="262144", app_type="UPDATE", owned=False))
                other_root = Libraries(path=str(root / "secondary"))
                db.session.add(other_root)
                db.session.flush()
                base = Apps.query.filter_by(app_id=reference.ALPHA).one()
                bundle = Files(library_id=other_root.id,
                               filepath=str(root / "secondary/AlphaBundle.xci"),
                               filename="AlphaBundle.xci", extension="xci", size=9000,
                               identified=True, multicontent=True, nb_content=2)
                bundle.apps.extend([base, newer])
                compressed = Files(library_id=other_root.id,
                                   filepath=str(root / "secondary/Alpha.nsz"),
                                   filename="Alpha.nsz", extension="nsz", size=750,
                                   identified=True, compressed=True)
                compressed.apps.append(base)
                db.session.add_all([bundle, compressed])
                db.session.flush()
                for file in Files.query.order_by(Files.id):
                    file.identification_type = "cnmt"
                    file.added_at = datetime.datetime(2026, 1, 1, 0, 0, file.id)
                    file.download_token = f"{file.id:032x}"
                for app in Apps.query:
                    app.display_version = f"fixture-{app.app_version}"
                db.session.commit()
                title_id = "0100000000010000"
                missing = Titles(title_id=title_id, have_base=True, up_to_date=True, complete=True)
                db.session.add(missing)
                db.session.flush()
                missing_app = Apps(title_id=missing.id, app_id=title_id, app_type="BASE",
                                   app_version="0", owned=True, display_version="0.9")
                db.session.add(missing_app)
                missing_file = Files(library_id=1, filepath=str(root / "games/Absent.nsp"),
                                     filename="Absent.nsp", extension="nsp", size=600,
                                     identified=True, identification_type="cnmt",
                                     added_at=datetime.datetime(2026, 1, 1, 0, 0, 7),
                                     download_token=f"{7:032x}")
                missing_file.apps.append(missing_app)
                db.session.add(missing_file)
                db.session.commit()
                titledb.store.set_extract_override(title_id,
                    {"id": title_id, "name": "Extracted Game", "publisher": "Extracted Publisher"}, 0)
                titledb.store.set_extract_override(reference.ALPHA,
                    {"id": reference.ALPHA, "name": "Extracted Alpha", "publisher": "Extracted Publisher"}, 131072)
                titledb.store.set_override(reference.ALPHA,
                    {"id": reference.ALPHA, "name": "Custom Alpha Game"})
                metadata = {reference.ALPHA: {"id": reference.ALPHA,
                            "name": "Custom Alpha Game", "publisher": "Nintendo"},
                            reference.ALPHA_DLC: reference.TITLEDB_JSON[reference.ALPHA_DLC],
                            title_id: {"id": title_id, "name": "Extracted Game",
                                       "publisher": "Extracted Publisher"}}
                titles = [{"id": t.id, "title_id": t.title_id, "have_base": t.have_base,
                           "complete": t.complete, "up_to_date": t.up_to_date}
                          for t in Titles.query.order_by(Titles.id)]
                apps = [{key: getattr(a, key) for key in
                         ("id", "title_id", "app_id", "app_type", "app_version", "owned", "display_version")}
                        for a in Apps.query.order_by(Apps.id)]
                files = [{**{key: getattr(f, key) for key in
                         ("id", "library_id", "filename", "extension", "size", "download_token",
                          "multicontent", "nb_content", "compressed", "organized", "identification_type")},
                         "added_at": f.added_at.isoformat(), "apps": [a.id for a in f.apps]}
                         for f in Files.query.order_by(Files.id)]
                cases = []
                for prefer in (False, True):
                    settings._cached_settings["library"]["management"]["deduplication"]["prefer_multicontent"] = prefer
                    for case in queries:
                        variables = dict(case["variables"], id=reference.ALPHA,
                                         titleIds=[reference.ALPHA], pageSize=2)
                        for page in (1, 2):
                            variables["page"] = page
                            result = schema.execute_sync(case["query"], variable_values=variables,
                                                         context_value=GraphQLContext(None, False, True))
                            if result.errors:
                                raise RuntimeError(str(result.errors))
                            cases.append({"name": case["name"], "prefer_multicontent": prefer,
                                          "query": case["query"], "variables": dict(variables),
                                          "data": result.data})
                fixture = {"upstream": PIN, "client": CLIENT, "source_sha256": hashes,
                           "titles": titles, "apps": apps, "files": files, "metadata": metadata,
                           "behavior": {"metadata_priority": list(SOURCE_PRIORITY),
                             "prefer_multicontent_default": False, "local_media_default": True,
                             "media_grace_seconds": media.COLLECT_GRACE,
                             "renditions": media.RENDITIONS,
                             "locales": {f"{region}.{language}": language_for_locale(region, language).name
                                         for region, language in (("US", "en"), ("GB", "en"),
                                             ("CA", "fr"), ("BR", "pt"), ("HK", "zh"), ("US", "es"),
                                             ("ES", "es"), ("JP", "unknown"))},
                             "fit_examples": [{"kind": kind, "size": size, "original": dimensions,
                                                "result": media.fit(dimensions, media.box(kind, size))}
                                              for kind in media.KINDS for size in media.RENDITIONS
                                              for dimensions in ((1920, 1080), (32, 16), (101, 997))]},
                           "cases": cases}
                output.parent.mkdir(parents=True, exist_ok=True)
                output.write_text(json.dumps(fixture, indent=2) + "\n")
                print(f"Captured {len(cases)} released-client responses to {output}")


if __name__ == "__main__":
    main()
