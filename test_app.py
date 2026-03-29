import json
import os
import time
from unittest.mock import MagicMock, patch

import pytest

import app as weather_app


@pytest.fixture(autouse=True)
def reset_globals():
    """Reset global caches between tests."""
    original_weather = weather_app.weather
    original_ecobee = weather_app.ecobee_home
    yield
    weather_app.weather = original_weather
    weather_app.ecobee_home = original_ecobee


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


ECOBEE_API_RESPONSE = {
    "thermostatList": [{
        "name": "Home",
        "runtime": {
            "connected": True,
            "actualTemperature": 722,
            "rawTemperature": 718,
            "actualHumidity": 45,
            "desiredHeat": 700,
            "desiredCool": 760,
            "desiredFanMode": "auto",
            "desiredHumidity": 40,
        },
        "settings": {
            "hvacMode": "heat",
        },
        "equipmentStatus": "fan",
        "weather": {
            "forecasts": [{
                "temperature": 480,
                "relativeHumidity": 60,
                "condition": "Cloudy",
                "dewpoint": 380,
                "windSpeed": 10,
                "windDirection": "NW",
            }]
        },
        "remoteSensors": [
            {
                "name": "Living Room",
                "type": "ecobee3_remote_sensor",
                "capability": [
                    {"type": "temperature", "value": "715"},
                    {"type": "occupancy", "value": "true"},
                ],
            },
            {
                "name": "Bedroom",
                "type": "ecobee3_remote_sensor",
                "capability": [
                    {"type": "temperature", "value": "705"},
                    {"type": "occupancy", "value": "false"},
                ],
            },
        ],
    }]
}


class TestGetAccessToken:
    def test_returns_none_when_no_tokens(self):
        with patch('app._load_tokens', return_value={}):
            assert weather_app._get_access_token() is None

    def test_returns_token_when_valid(self):
        tokens = {'access_token': 'mytoken', 'expires_at': time.time() + 3600}
        with patch('app._load_tokens', return_value=tokens):
            assert weather_app._get_access_token() == 'mytoken'

    def test_refreshes_when_expired(self):
        tokens = {
            'access_token': 'oldtoken',
            'refresh_token': 'myrefresh',
            'expires_at': time.time() - 1,
        }
        refreshed = {'access_token': 'newtoken', 'refresh_token': 'newrefresh', 'expires_in': 3600}
        mock_response = MagicMock()
        mock_response.status_code = 200
        mock_response.json.return_value = refreshed
        with patch('app._load_tokens', return_value=tokens), \
             patch('app._save_tokens') as mock_save, \
             patch('app.requests.post', return_value=mock_response), \
             patch.dict(os.environ, {'ECOBEE_API_KEY': 'myapikey'}):
            result = weather_app._get_access_token()
        assert result == 'newtoken'
        saved = mock_save.call_args[0][0]
        assert saved['access_token'] == 'newtoken'
        assert 'expires_at' in saved

    def test_returns_none_when_refresh_fails(self):
        tokens = {'access_token': 'oldtoken', 'refresh_token': 'myrefresh', 'expires_at': 0}
        mock_response = MagicMock()
        mock_response.status_code = 500
        with patch('app._load_tokens', return_value=tokens), \
             patch('app.requests.post', return_value=mock_response):
            assert weather_app._get_access_token() is None

    def test_refresh_uses_api_key_and_refresh_token(self):
        tokens = {'access_token': 'old', 'refresh_token': 'myrefresh', 'expires_at': 0}
        refreshed = {'access_token': 'new', 'refresh_token': 'newrefresh', 'expires_in': 3600}
        mock_response = MagicMock()
        mock_response.status_code = 200
        mock_response.json.return_value = refreshed
        with patch('app._load_tokens', return_value=tokens), \
             patch('app._save_tokens'), \
             patch('app.requests.post', return_value=mock_response) as mock_post, \
             patch.dict(os.environ, {'ECOBEE_API_KEY': 'myapikey'}):
            weather_app._get_access_token()
            params = mock_post.call_args[1]['params']
        assert params['grant_type'] == 'refresh_token'
        assert params['refresh_token'] == 'myrefresh'
        assert params['client_id'] == 'myapikey'


class TestEcobeeAuthorizeEndpoint:
    def _make_mock_response(self, status_code=200):
        r = MagicMock()
        r.status_code = status_code
        r.json.return_value = {'ecobeePin': 'ab12', 'code': 'authcode123', 'expires_in': 900}
        r.text = 'error'
        return r

    def test_returns_pin_and_instructions(self, client):
        with patch('app.requests.get', return_value=self._make_mock_response()), \
             patch('app._save_tokens'):
            data = json.loads(client.get('/ecobee_authorize').data)
        assert data['pin'] == 'ab12'
        assert 'instructions' in data
        assert data['expires_in_seconds'] == 900

    def test_saves_pending_code(self, client):
        with patch('app.requests.get', return_value=self._make_mock_response()), \
             patch('app._save_tokens') as mock_save:
            client.get('/ecobee_authorize')
        mock_save.assert_called_once_with({'pending_code': 'authcode123'})

    def test_uses_api_key_from_env(self, client):
        with patch('app.requests.get', return_value=self._make_mock_response()) as mock_get, \
             patch('app._save_tokens'), \
             patch.dict(os.environ, {'ECOBEE_API_KEY': 'myapikey'}):
            client.get('/ecobee_authorize')
            params = mock_get.call_args[1]['params']
        assert params['client_id'] == 'myapikey'

    def test_error_from_ecobee_propagates(self, client):
        with patch('app.requests.get', return_value=self._make_mock_response(status_code=500)):
            assert client.get('/ecobee_authorize').status_code == 500


class TestEcobeeCompleteAuthEndpoint:
    def _make_mock_token_response(self, status_code=200):
        r = MagicMock()
        r.status_code = status_code
        r.json.return_value = {
            'access_token': 'myaccesstoken',
            'refresh_token': 'myrefreshtoken',
            'expires_in': 3600,
        }
        r.text = 'error'
        return r

    def test_exchanges_code_for_tokens(self, client):
        with patch('app._load_tokens', return_value={'pending_code': 'authcode123'}), \
             patch('app._save_tokens') as mock_save, \
             patch('app.requests.post', return_value=self._make_mock_token_response()):
            assert client.get('/ecobee_complete_auth').status_code == 200
        saved = mock_save.call_args[0][0]
        assert saved['access_token'] == 'myaccesstoken'
        assert saved['refresh_token'] == 'myrefreshtoken'
        assert 'expires_at' in saved

    def test_no_pending_code_returns_400(self, client):
        with patch('app._load_tokens', return_value={}):
            assert client.get('/ecobee_complete_auth').status_code == 400

    def test_error_from_ecobee_propagates(self, client):
        with patch('app._load_tokens', return_value={'pending_code': 'authcode123'}), \
             patch('app.requests.post', return_value=self._make_mock_token_response(status_code=500)):
            assert client.get('/ecobee_complete_auth').status_code == 500

    def test_uses_api_key_and_code(self, client):
        with patch('app._load_tokens', return_value={'pending_code': 'authcode123'}), \
             patch('app._save_tokens'), \
             patch('app.requests.post', return_value=self._make_mock_token_response()) as mock_post, \
             patch.dict(os.environ, {'ECOBEE_API_KEY': 'myapikey'}):
            client.get('/ecobee_complete_auth')
            params = mock_post.call_args[1]['params']
        assert params['grant_type'] == 'ecobeePin'
        assert params['code'] == 'authcode123'
        assert params['client_id'] == 'myapikey'


class TestEcobeeHomeEndpoint:
    def test_returns_200(self, client):
        assert client.get('/ecobee_home').status_code == 200

    def test_returns_cached_data(self, client):
        weather_app.ecobee_home = {'name': 'Home', 'indoor': {'temperature': 72.2}}
        data = json.loads(client.get('/ecobee_home').data)
        assert data['name'] == 'Home'
        assert data['indoor']['temperature'] == 72.2

    def test_returns_empty_when_not_yet_fetched(self, client):
        weather_app.ecobee_home = {}
        assert client.get('/ecobee_home').status_code == 200


class TestUpdateEcobeeHome:
    def _make_mock_response(self, status_code=200, body=ECOBEE_API_RESPONSE):
        r = MagicMock()
        r.status_code = status_code
        r.json.return_value = body
        return r

    def test_skips_update_when_no_token(self):
        with patch('app._get_access_token', return_value=None), \
             patch('app.requests.get') as mock_get:
            weather_app.update_ecobee_home()
        mock_get.assert_not_called()

    def test_success_updates_cache(self):
        with patch('app._get_access_token', return_value='mytoken'), \
             patch('app.requests.get', return_value=self._make_mock_response()):
            weather_app.update_ecobee_home()
        h = weather_app.ecobee_home
        assert h['name'] == 'Home'
        assert h['connected'] is True
        assert h['hvac_mode'] == 'heat'
        assert h['equipment_status'] == 'fan'
        assert h['indoor'] == {'temperature': 72.2, 'raw_temperature': 71.8, 'humidity': 45}
        assert h['outdoor'] == {
            'temperature': 48.0, 'humidity': 60, 'condition': 'Cloudy',
            'dewpoint': 38.0, 'wind_speed': 10, 'wind_direction': 'NW',
        }
        assert h['setpoints'] == {'heat': 70.0, 'cool': 76.0, 'fan_mode': 'auto', 'humidity': 40}
        assert h['sensors'] == [
            {'name': 'Living Room', 'type': 'ecobee3_remote_sensor', 'temperature': 71.5, 'occupancy': True},
            {'name': 'Bedroom', 'type': 'ecobee3_remote_sensor', 'temperature': 70.5, 'occupancy': False},
        ]

    def test_indoor_temperature_converted_from_tenths(self):
        t = ECOBEE_API_RESPONSE['thermostatList'][0]
        body = {'thermostatList': [{**t, 'runtime': {**t['runtime'], 'actualTemperature': 685}}]}
        with patch('app._get_access_token', return_value='mytoken'), \
             patch('app.requests.get', return_value=self._make_mock_response(body=body)):
            weather_app.update_ecobee_home()
        assert weather_app.ecobee_home['indoor']['temperature'] == 68.5

    def test_outdoor_temperature_converted_from_tenths(self):
        t = ECOBEE_API_RESPONSE['thermostatList'][0]
        body = {'thermostatList': [{**t, 'weather': {'forecasts': [{**t['weather']['forecasts'][0], 'temperature': 325}]}}]}
        with patch('app._get_access_token', return_value='mytoken'), \
             patch('app.requests.get', return_value=self._make_mock_response(body=body)):
            weather_app.update_ecobee_home()
        assert weather_app.ecobee_home['outdoor']['temperature'] == 32.5

    def test_sensor_unknown_temperature_omitted(self):
        t = ECOBEE_API_RESPONSE['thermostatList'][0]
        body = {'thermostatList': [{**t, 'remoteSensors': [
            {'name': 'Garage', 'type': 'ecobee3_remote_sensor',
             'capability': [{'type': 'temperature', 'value': 'unknown'}]},
        ]}]}
        with patch('app._get_access_token', return_value='mytoken'), \
             patch('app.requests.get', return_value=self._make_mock_response(body=body)):
            weather_app.update_ecobee_home()
        assert 'temperature' not in weather_app.ecobee_home['sensors'][0]

    def test_error_preserves_existing_cache(self):
        weather_app.ecobee_home = {'name': 'Home', 'indoor': {'temperature': 70.0}}
        with patch('app._get_access_token', return_value='mytoken'), \
             patch('app.requests.get', return_value=self._make_mock_response(status_code=500)):
            weather_app.update_ecobee_home()
        assert weather_app.ecobee_home == {'name': 'Home', 'indoor': {'temperature': 70.0}}

    def test_passes_access_token_as_bearer(self):
        with patch('app._get_access_token', return_value='mytoken'), \
             patch('app.requests.get', return_value=self._make_mock_response()) as mock_get:
            weather_app.update_ecobee_home()
            headers = mock_get.call_args[1]['headers']
        assert headers['Authorization'] == 'Bearer mytoken'

    def test_calls_correct_url(self):
        with patch('app._get_access_token', return_value='mytoken'), \
             patch('app.requests.get', return_value=self._make_mock_response()) as mock_get:
            weather_app.update_ecobee_home()
            url = mock_get.call_args[0][0]
        assert 'api.ecobee.com' in url
        assert 'thermostat' in url

    def test_equipment_status_defaults_to_empty_when_absent(self):
        t = ECOBEE_API_RESPONSE['thermostatList'][0]
        body = {'thermostatList': [{k: v for k, v in t.items() if k != 'equipmentStatus'}]}
        with patch('app._get_access_token', return_value='mytoken'), \
             patch('app.requests.get', return_value=self._make_mock_response(body=body)):
            weather_app.update_ecobee_home()
        assert weather_app.ecobee_home['equipment_status'] == ''
