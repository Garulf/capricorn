self:
{ pkgs, ... }:

{
  name = "capricorn";

  nodes.machine =
    { pkgs, ... }:
    {
      imports = [ self.nixosModules.default ];

      users.users.alice = {
        isNormalUser = true;
        uid = 1000;
      };
      users.users.mallory = {
        isNormalUser = true;
        uid = 1001;
      };

      services.capricorn = {
        enable = true;
        users = [ "alice" ];
        # The VM has no LUKS; the swap/hibernation/coredump checks still run.
        settings.kdf = {
          memory_kib = 8192;
          iterations = 1;
          parallelism = 1;
        };
      };

      virtualisation.memorySize = 2048;
    };

  testScript = ''
    import shlex

    def as_user(user, cmd):
        return machine.succeed(f"su - {user} -c {shlex.quote(cmd)}")

    def as_user_fails(user, cmd):
        return machine.fail(f"su - {user} -c {shlex.quote(cmd)}")

    machine.wait_for_unit("capricornd.service")
    machine.wait_for_file("/run/capricorn/client.sock")
    machine.wait_for_file("/run/capricorn/capture.sock")

    with subtest("daemon has no network"):
        pid = machine.succeed("systemctl show -p MainPID --value capricornd").strip()
        links = machine.succeed(f"nsenter -t {pid} -n ip -o link")
        assert links.strip().count("\n") == 0 and ": lo:" in links, links

    with subtest("data dir is private"):
        machine.succeed("test \"$(stat -c '%U %a' /var/lib/capricorn)\" = 'capricorn 700'")
        as_user_fails("alice", "ls /var/lib/capricorn")

    with subtest("init and capture while locked"):
        as_user("alice", "printf 'correct horse\\n' | capricorn init --password-stdin --no-recovery")
        as_user("alice", "capricorn lock")
        as_user("alice", "capricorn-capture inject --clipboard 'CANARY-CLIP-1234 nix flake update' --class kitty --title shell")
        machine.sleep(2)
        machine.fail("grep -r CANARY-CLIP /var/lib/capricorn")

    with subtest("search after unlock in one session"):
        search = "printf 'correct horse\\n' | capricorn search CANARY --password-stdin --json"
        machine.wait_until_succeeds(
            f"su - alice -c {shlex.quote(search)} | grep -F CANARY-CLIP-1234", timeout=120
        )

    with subtest("wrong password is refused"):
        as_user_fails("alice", "printf 'wrong\\n' | capricorn search CANARY --password-stdin --json")

    with subtest("no plaintext after lock"):
        as_user("alice", "capricorn lock")
        machine.sleep(1)
        machine.fail("grep -r CANARY-CLIP /var/lib/capricorn")

    with subtest("users outside the group cannot connect"):
        as_user_fails("mallory", "capricorn status")
  '';
}
