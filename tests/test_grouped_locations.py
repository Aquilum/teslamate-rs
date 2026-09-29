"""Browser regression for grouped locations and its admin-only controls.

Run against a disposable local SQLite database with:
    python3 -m unittest discover -s tests -p 'test_*.py'

Requires Selenium and a Chrome/Chromium or Firefox WebDriver. Set
TESLAMATE_RS_BIN and SELENIUM_BROWSER (chrome or firefox) as needed.
"""

import os
import socket
import subprocess
import tempfile
import time
import unittest
from pathlib import Path
from urllib.request import urlopen

from selenium import webdriver
from selenium.common.exceptions import StaleElementReferenceException
from selenium.webdriver import ActionChains
from selenium.webdriver.common.by import By
from selenium.webdriver.support.ui import WebDriverWait


ROOT = Path(__file__).resolve().parents[1]


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


class GroupedLocationsBrowserTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.temp = tempfile.TemporaryDirectory(prefix="teslamate-selenium-")
        binary = Path(os.environ.get("TESLAMATE_RS_BIN", ROOT / "target/debug/teslamate-rs"))
        if not binary.is_file():
            cls.temp.cleanup()
            raise RuntimeError(f"Build teslamate-rs first or set TESLAMATE_RS_BIN: {binary}")
        port = free_port()
        cls.base = f"http://127.0.0.1:{port}"
        env = {**os.environ, "TESLAMATE_RS_HOME": cls.temp.name, "RUST_LOG": "error"}
        cls.server = subprocess.Popen(
            [str(binary), "--db", str(Path(cls.temp.name) / "test.sqlite"),
             "serve", "--auth", "local", "--no-logger", "--port", str(port)],
            cwd=ROOT, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE,
        )
        for _ in range(100):
            if cls.server.poll() is not None:
                error = cls.server.stderr.read().decode(errors="replace")
                cls.temp.cleanup()
                raise RuntimeError(f"Test server exited: {error}")
            try:
                with urlopen(cls.base + "/api/health", timeout=0.2) as response:
                    if response.status == 200:
                        break
            except OSError:
                time.sleep(0.1)
        else:
            cls.server.terminate()
            cls.server.wait(timeout=5)
            cls.temp.cleanup()
            raise RuntimeError("Test server did not become healthy")

        browser = os.environ.get("SELENIUM_BROWSER", "chrome").lower()
        try:
            if browser == "firefox":
                options = webdriver.FirefoxOptions()
                options.add_argument("-headless")
                cls.driver = webdriver.Firefox(options=options)
            elif browser == "chrome":
                options = webdriver.ChromeOptions()
                options.add_argument("--headless=new")
                options.add_argument("--no-sandbox")
                options.add_argument("--disable-dev-shm-usage")
                cls.driver = webdriver.Chrome(options=options)
            else:
                raise ValueError("SELENIUM_BROWSER must be chrome or firefox")
        except Exception:
            cls.server.terminate()
            cls.server.wait(timeout=5)
            cls.temp.cleanup()
            raise
        cls.driver.set_window_size(1440, 900)

    @classmethod
    def tearDownClass(cls):
        cls.driver.quit()
        cls.server.terminate()
        cls.server.wait(timeout=5)
        cls.server.stderr.close()
        cls.temp.cleanup()

    def wait_for(self, selector, visible=True):
        def found(driver):
            matches = driver.find_elements(By.CSS_SELECTOR, selector)
            return next((item for item in matches if not visible or item.is_displayed()), False)
        return WebDriverWait(self.driver, 15, ignored_exceptions=(StaleElementReferenceException,)).until(found)

    def open_account(self):
        if "open" not in self.driver.find_element(By.ID, "account").get_attribute("class").split():
            self.driver.find_element(By.ID, "account-btn").click()

    def test_admin_edit_and_regular_user_read_only(self):
        driver = self.driver
        driver.get(self.base + "/#locations")
        self.wait_for("#card-setup")
        driver.find_element(By.ID, "setup-user").send_keys("admin")
        driver.find_element(By.ID, "setup-pass").send_keys("correct horse battery staple")
        driver.find_element(By.CSS_SELECTOR, "#card-setup button[type=submit]").click()
        self.wait_for("#app")
        self.open_account()
        driver.find_element(By.ID, "ui-layout").find_element(By.CSS_SELECTOR, "option[value=grouped]").click()
        WebDriverWait(driver, 10).until(lambda d: d.execute_script("return location.hash") == "#vehicle")
        driver.get(self.base + "/#locations")
        self.wait_for("#location-form")
        map_element = self.wait_for("#location-map")
        ActionChains(driver).move_to_element(map_element).click().perform()
        WebDriverWait(driver, 5).until(
            lambda d: d.find_element(By.CSS_SELECTOR, "#location-form [name=latitude]").get_attribute("value")
        )
        driver.find_element(By.CSS_SELECTOR, "#location-form [name=name]").send_keys("Home")
        driver.find_element(By.CSS_SELECTOR, "#location-form [name=cost_per_unit]").send_keys("0.25")
        driver.find_element(By.CSS_SELECTOR, "#location-form button[type=submit]").click()
        WebDriverWait(driver, 15, ignored_exceptions=(StaleElementReferenceException,)).until(
            lambda d: "Home" in d.find_element(By.ID, "location-rows").text
        )
        self.assertTrue(driver.find_element(By.CSS_SELECTOR, "#location-rows [data-edit]").is_displayed())

        self.open_account()
        driver.find_element(By.ID, "invite-btn").click()
        invite = WebDriverWait(driver, 10).until(
            lambda d: d.find_element(By.ID, "invite-url").text
            if "#register?invite=" in d.find_element(By.ID, "invite-url").text else False
        )
        driver.find_element(By.ID, "logout-btn").click()
        self.wait_for("#card-login")
        # Force a document navigation: the signed-in hash listener still exists
        # until reload and would otherwise redirect the invite hash.
        driver.get(invite.replace("/#", "/?selenium=register#"))
        self.wait_for("#card-register")
        driver.find_element(By.ID, "reg-user").send_keys("viewer")
        driver.find_element(By.ID, "reg-pass").send_keys("another correct horse")
        driver.find_element(By.CSS_SELECTOR, "#card-register button[type=submit]").click()
        self.wait_for("#app")
        self.open_account()
        driver.find_element(By.ID, "ui-layout").find_element(By.CSS_SELECTOR, "option[value=grouped]").click()
        WebDriverWait(driver, 10).until(lambda d: d.execute_script("return location.hash") == "#vehicle")
        driver.get(self.base + "/#locations")
        self.wait_for("#location-map")
        WebDriverWait(driver, 15, ignored_exceptions=(StaleElementReferenceException,)).until(
            lambda d: "Home" in d.find_element(By.ID, "location-rows").text
        )
        self.assertFalse(driver.find_element(By.ID, "location-form").is_displayed())
        self.assertFalse(driver.find_element(By.ID, "locations-rematch").is_displayed())
        self.assertFalse(driver.find_elements(By.CSS_SELECTOR, "#location-rows [data-edit], #location-rows [data-delete]"))
        self.assertEqual(403, driver.execute_async_script("""
            const done = arguments[arguments.length - 1];
            fetch('/api/geofences/rematch', {method: 'POST'}).then(r => done(r.status));
        """))


if __name__ == "__main__":
    unittest.main()
