import json
import os
import time
from unittest.mock import MagicMock, patch

import pytest

import app as weather_app


@pytest.fixture(autouse=True)
def reset_weather():
    """Reset global weather cache between tests."""
    original = weather_app.weather
    yield
    weather_app.weather = original


@pytest.fixture
def client():
    weather_app.app.config['TESTING'] = True
    with weather_app.app.test_client() as c:
        yield c


class TestEpochEndpoint:
    def test_returns_200(self, client):
        assert client.get('/epoch').status_code == 200

    def test_returns_integer_json(self, client):
        data = json.loads(client.get('/epoch').data)
        assert isinstance(data, int)

    def test_returns_current_time(self, client):
        before = int(time.time())
        data = json.loads(client.get('/epoch').data)
        after = int(time.time())
        assert before <= data <= after


class TestWeatherEndpointBackwardsCompatibility:
    def test_returns_200(self, client):
        assert client.get('/').status_code == 200


class TestOwmOneshotEndpoint:
    def test_returns_200(self, client):
        assert client.get('/owm_oneshot').status_code == 200

    def test_returns_cached_weather(self, client):
        weather_app.weather = '{"temp": 72}'
        assert client.get('/owm_oneshot').data == b'{"temp": 72}'

    def test_returns_empty_when_not_yet_fetched(self, client):
        weather_app.weather = {}
        assert client.get('/owm_oneshot').status_code == 200


class TestUpdateWeather:
    def _make_mock_response(self, status_code=200, text='{"weather": "sunny"}'):
        r = MagicMock()
        r.status_code = status_code
        r.text = text
        return r

    def test_success_updates_cache(self):
        with patch('app.requests.get', return_value=self._make_mock_response()):
            weather_app.update_weather()
        assert weather_app.weather == '{"weather": "sunny"}'

    def test_error_preserves_existing_cache(self):
        weather_app.weather = '{"original": "data"}'
        with patch('app.requests.get', return_value=self._make_mock_response(status_code=500)):
            weather_app.update_weather()
        assert weather_app.weather == '{"original": "data"}'

    def test_passes_env_vars_as_params(self):
        env = {'LAT': '37.7', 'LON': '-122.4', 'APP_ID': 'testkey', 'UNITS': 'metric'}
        with patch('app.requests.get', return_value=self._make_mock_response()) as mock_get, \
             patch.dict(os.environ, env):
            weather_app.update_weather()
            params = mock_get.call_args[1]['params']
        assert params['lat'] == '37.7'
        assert params['lon'] == '-122.4'
        assert params['appid'] == 'testkey'
        assert params['units'] == 'metric'

    def test_units_defaults_to_imperial(self):
        env = {'LAT': '0', 'LON': '0', 'APP_ID': 'key'}
        with patch('app.requests.get', return_value=self._make_mock_response()) as mock_get, \
             patch.dict(os.environ, env, clear=True):
            weather_app.update_weather()
            params = mock_get.call_args[1]['params']
        assert params['units'] == 'imperial'

    def test_calls_correct_url(self):
        with patch('app.requests.get', return_value=self._make_mock_response()) as mock_get:
            weather_app.update_weather()
            url = mock_get.call_args[0][0]
        assert 'openweathermap.org' in url
        assert 'onecall' in url
